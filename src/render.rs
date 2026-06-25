//! 渲染层：wgpu 上屏 + glyphon 画文字 + 一条「实心矩形」管线画背景/光标/选区/下划线/tab 条。
//!
//! 读 `Grid`（含 scrollback 回看），把每个可见单元格画成
//!   背景色矩形（矩形管线）＋ 前景字形（glyphon/cosmic-text，支持 **bold/italic/dim/underline/truecolor**）。
//! 块状光标 = reverse 渲染光标格；选区 = 叠一层选区底色；下划线 = 单元格底部一条细矩形。
//! 顶部 **tab 条**（>1 标签时显示）画每个会话的状态点 + 标题 + 完成角标。
//!
//! 颜色空间：刻意选 **非 sRGB（Unorm）** 交换链格式 + glyphon `ColorMode::Web`，
//! 让矩形与文字都「按 0–255 当 sRGB 直接写」，truecolor 所见即所得。
//!
//! 版本：winit 0.30 / wgpu 29 / glyphon 0.11（cosmic-text 0.18）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, ColorMode, Family, FontSystem, Metrics, Resolution,
    Shaping, Style, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::grid::{char_width, Grid, Selection, Theme};
use crate::window::WinStatus;

/// 侧边栏的一行（App 每帧 clone 一份，避免与 grid 借用冲突）。
/// `is_host` = host 分组头（粗体、不缩进、不画状态点）；否则是窗口行（缩进、画状态/角标）。
pub struct SidebarItem {
    pub label: String,
    pub is_host: bool,
    pub status: WinStatus,
    pub activity: bool,
    pub alerted: bool,
    pub active: bool,
}

/// 偏好面板视图（App 每帧构造）：标题 + 若干「标签: 值」行 + 选中行。
pub struct PrefView {
    pub title: String,
    pub rows: Vec<(String, String)>,
    pub selected: usize,
}

/// 矩形管线的顶点：裁剪空间坐标 + RGB（0–1）。
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    pos: [f32; 2],
    color: [f32; 3],
}

const RECT_SHADER: &str = r#"
struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) color: vec3<f32>) -> VSOut {
    var out: VSOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VSOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;

/// 一行里同 (前景色,粗,斜) 的一段，已 shape 好的 glyphon Buffer + 像素左边界与颜色。
struct RunBuf {
    buffer: Buffer,
    left: f32,
    color: GColor,
}

/// 一行的缓存：内容哈希 + 这一行 shape 好的若干段。
struct CachedLine {
    hash: u64,
    runs: Vec<RunBuf>,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,

    rect_pipeline: wgpu::RenderPipeline,

    line_cache: Vec<Option<CachedLine>>,

    font_size: f32,
    cell_w: f32,
    cell_h: f32,
    pad: f32,
    scale: f32,
    n_tabs: usize,
    sidebar_on: bool,
    pub theme: Theme,

    instance: wgpu::Instance,
    window: Arc<Window>,
}

impl Renderer {
    pub fn new(window: Arc<Window>, font_size: Option<f32>, theme: Theme) -> anyhow::Result<Self> {
        pollster::block_on(Self::new_async(window, font_size, theme))
    }

    async fn new_async(
        window: Arc<Window>,
        font_size_cfg: Option<f32>,
        theme: Theme,
    ) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| anyhow::anyhow!("找不到可用的 GPU adapter: {e}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("term-device"),
                ..Default::default()
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(
                    f,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            })
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        let font_size = font_size_cfg.unwrap_or(15.0).max(6.0) * scale;
        let cell_h = (font_size * 1.25).ceil();
        let cell_w = measure_cell_width(&mut font_system, font_size, cell_h);
        let pad = (4.0 * scale).round();

        let rect_pipeline = build_rect_pipeline(&device, format);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            rect_pipeline,
            line_cache: Vec::new(),
            font_size,
            cell_w,
            cell_h,
            pad,
            scale,
            n_tabs: 1,
            sidebar_on: false,
            theme,
            instance,
            window,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.line_cache.clear();
    }

    /// 更新布局参数（窗口数 + 是否显示侧边栏）。**必须在 `cols_rows()` 算尺寸前调用**，
    /// 否则开/关侧边栏时 PTY 会按旧的可用宽度算列数，导致差一帧错位。
    pub fn set_layout(&mut self, n_windows: usize, show_sidebar: bool) {
        self.n_tabs = n_windows.max(1);
        self.sidebar_on = show_sidebar;
    }

    /// 设置 / 更新字号（偏好面板调字号时用），重算单元格度量并清缓存。
    pub fn set_font_size(&mut self, logical: f32, scale: f32) {
        self.font_size = logical.clamp(6.0, 48.0) * scale;
        self.cell_h = (self.font_size * 1.25).ceil();
        self.cell_w = measure_cell_width(&mut self.font_system, self.font_size, self.cell_h);
        self.pad = (4.0 * scale).round();
        self.line_cache.clear();
    }

    /// 换主题（偏好面板切配色用），清行缓存（颜色变了要重排）。
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.line_cache.clear();
    }

    /// 侧边栏宽度（像素）；不显示则 0。
    fn sidebar_w(&self) -> f32 {
        if self.sidebar_on {
            (self.cell_w * 18.0)
                .round()
                .clamp(self.cell_w * 8.0, self.config.width as f32 * 0.4)
        } else {
            0.0
        }
    }
    /// 侧边栏每行高度。
    fn row_h(&self) -> f32 {
        (self.cell_h * 1.3).round()
    }
    /// 终端区域左边界（侧边栏之右）。
    fn term_left(&self) -> f32 {
        self.sidebar_w() + self.pad
    }
    fn term_top(&self) -> f32 {
        self.pad
    }

    /// 当前像素尺寸能容纳的 (列, 行)（扣掉侧边栏）。
    pub fn cols_rows(&self) -> (usize, usize) {
        let usable_w =
            (self.config.width as f32 - self.sidebar_w() - 2.0 * self.pad).max(self.cell_w);
        let usable_h = (self.config.height as f32 - 2.0 * self.pad).max(self.cell_h);
        let cols = (usable_w / self.cell_w).floor() as usize;
        let rows = (usable_h / self.cell_h).floor() as usize;
        (cols.max(1), rows.max(1))
    }

    /// 像素坐标 → 终端 (列, 行)（用于鼠标选择 / 鼠标上报）。
    pub fn cell_at(&self, x: f64, y: f64) -> (usize, usize) {
        let (cols, rows) = self.cols_rows();
        let cx = ((x as f32 - self.term_left()) / self.cell_w).floor();
        let cy = ((y as f32 - self.term_top()) / self.cell_h).floor();
        let col = if cx < 0.0 { 0 } else { cx as usize };
        let row = if cy < 0.0 { 0 } else { cy as usize };
        (col.min(cols - 1), row.min(rows - 1))
    }

    /// 像素坐标落在侧边栏第几行。None = 不在侧边栏。
    pub fn sidebar_row_at(&self, x: f64, y: f64) -> Option<usize> {
        if !self.sidebar_on || (x as f32) >= self.sidebar_w() {
            return None;
        }
        let r = ((y as f32 - self.pad) / self.row_h()).floor();
        if r < 0.0 {
            return None;
        }
        Some(r as usize)
    }

    /// 画一帧：左侧边栏 + 背景/选区/光标矩形 + 文字 + 下划线 +（可选）偏好面板浮层。
    /// （窗口数 / 是否显示侧边栏由 App 先 `set_layout` 设好。）
    pub fn render(
        &mut self,
        grid: &Grid,
        selection: Option<Selection>,
        sidebar: &[SidebarItem],
        pref: Option<&PrefView>,
    ) {
        let pref_open = pref.is_some();
        let (cols, rows) = (grid.cols, grid.rows);
        let width = self.config.width as f32;
        let height = self.config.height as f32;
        let term_top = self.term_top();
        let term_left = self.term_left();
        let show_cursor = grid.modes.cursor_visible && grid.view_offset == 0;

        let is_cursor =
            |col: usize, row: usize| show_cursor && row == grid.cy && col == grid.cx;
        let is_sel = |col: usize, row: usize| selection.is_some_and(|s| s.contains(col, row));

        // 计算某格的有效背景：光标格用光标色（块状光标），选区叠选区色，否则单元格背景。
        let cell_bg = |col: usize, row: usize| -> [u8; 3] {
            if is_cursor(col, row) {
                self.theme.cursor
            } else if is_sel(col, row) {
                self.theme.selection
            } else {
                grid.visible_cell(col, row).effective_colors(&self.theme, false).1
            }
        };

        // ---- 1) 矩形：tab 条 + 终端背景 + 下划线 ----
        let mut verts: Vec<RectVertex> = Vec::new();

        // 终端背景（按行扫描，合并同色段）。
        for row in 0..rows {
            let top = term_top + row as f32 * self.cell_h;
            let mut col = 0;
            while col < cols {
                let bg0 = cell_bg(col, row);
                let start = col;
                col += 1;
                while col < cols && cell_bg(col, row) == bg0 {
                    col += 1;
                }
                if bg0 != self.theme.bg {
                    let x0 = term_left + start as f32 * self.cell_w;
                    let x1 = term_left + col as f32 * self.cell_w;
                    push_rect(&mut verts, x0, top, x1, top + self.cell_h, bg0, width, height);
                }
            }
            // 下划线：本行每个 underline 单元格底部一条细线。
            let uy = top + self.cell_h - (2.0 * (self.cell_h / 20.0)).max(1.0);
            let mut c = 0;
            while c < cols {
                let cell = grid.visible_cell(c, row);
                if cell.underline && !cell.wide_spacer {
                    let (fg, _) = cell.effective_colors(&self.theme, is_cursor(c, row));
                    let cw = if char_width(cell.c) == 2 { 2 } else { 1 };
                    let x0 = term_left + c as f32 * self.cell_w;
                    let x1 = term_left + (c + cw) as f32 * self.cell_w; // 宽字符下划线跨两列
                    push_rect(&mut verts, x0, uy, x1, uy + (self.cell_h / 16.0).max(1.0), fg, width, height);
                }
                c += 1;
            }
        }

        // 左侧边栏：host 分组头 + 缩进的窗口行（状态点 / 角标 / 活动高亮）。
        let mut label_bufs: Vec<(Buffer, f32, f32, GColor)> = Vec::new();
        let sw = self.sidebar_w();
        if sw > 0.0 {
            let bar_bg = darken(self.theme.bg, 0.6);
            push_rect(&mut verts, 0.0, 0.0, sw, height, bar_bg, width, height);
            // 右侧分隔线。
            let div = (1.0 * self.scale).max(1.0);
            push_rect(&mut verts, sw - div, 0.0, sw, height, darken(self.theme.fg, 0.25), width, height);

            let row_h = self.row_h();
            let ds = (self.cell_h * 0.16).max(2.5);
            for (i, item) in sidebar.iter().enumerate() {
                let y0 = self.pad + i as f32 * row_h;
                let y1 = y0 + row_h;
                if y0 >= height {
                    break; // 超出可视高度
                }
                if item.active {
                    push_rect(&mut verts, 0.0, y0, sw - div, y1, self.theme.selection, width, height);
                }
                let midy = (y0 + y1) * 0.5;
                // host 头不缩进；窗口行缩进 + 状态点。
                let mut tx = if item.is_host {
                    self.pad
                } else {
                    self.pad + 1.5 * self.cell_w
                };
                if !item.is_host {
                    let dot = status_color(item.status, item.alerted);
                    let dx = tx + ds;
                    push_rect(&mut verts, dx - ds, midy - ds, dx + ds, midy + ds, dot, width, height);
                    tx = dx + ds + 0.4 * self.cell_w;
                }
                let label_color = if item.is_host {
                    // host 头用偏亮强调色（青）。
                    GColor::rgb(self.theme.ansi[6][0], self.theme.ansi[6][1], self.theme.ansi[6][2])
                } else if item.active || item.activity {
                    GColor::rgb(self.theme.fg[0], self.theme.fg[1], self.theme.fg[2])
                } else {
                    let d = darken(self.theme.fg, 0.55);
                    GColor::rgb(d[0], d[1], d[2])
                };
                let avail = sw - tx - self.pad;
                let max_chars = (avail / self.cell_w).floor().max(1.0) as usize;
                let label = truncate(&item.label, max_chars);
                let buf = shape_label(&mut self.font_system, &label, self.font_size, self.cell_h, label_color);
                let ly = midy - self.cell_h * 0.5;
                label_bufs.push((buf, tx, ly, label_color));
            }
        }

        // 偏好面板浮层（模态）：暗化全屏 + 居中面板框 + 行。
        let mut panel_bufs: Vec<(Buffer, f32, f32, GColor)> = Vec::new();
        if let Some(pv) = pref {
            // 暗化整屏。
            push_rect(&mut verts, 0.0, 0.0, width, height, darken(self.theme.bg, 0.45), width, height);
            let line_h = (self.cell_h * 1.6).round();
            let pw = (self.cell_w * 46.0).min(width * 0.92);
            let ph = line_h * (pv.rows.len() as f32 + 2.5);
            let px = ((width - pw) * 0.5).round();
            let py = ((height - ph) * 0.5).round();
            // 边框（强调色外框 + 面板底内框）。
            let acc = self.theme.ansi[4];
            let b = (2.0 * self.scale).max(1.5);
            push_rect(&mut verts, px - b, py - b, px + pw + b, py + ph + b, acc, width, height);
            push_rect(&mut verts, px, py, px + pw, py + ph, darken(self.theme.bg, 0.9), width, height);
            // 标题。
            let tcol = GColor::rgb(acc[0], acc[1], acc[2]);
            let tb = shape_label(&mut self.font_system, &pv.title, self.font_size, self.cell_h, tcol);
            panel_bufs.push((tb, px + self.cell_w, py + line_h * 0.4, tcol));
            // 行：左标签 + 右值；选中行高亮。
            for (i, (label, value)) in pv.rows.iter().enumerate() {
                let ry = py + line_h * (i as f32 + 1.6);
                if i == pv.selected {
                    push_rect(&mut verts, px + 0.5 * self.cell_w, ry - line_h * 0.1, px + pw - 0.5 * self.cell_w, ry + line_h * 0.9, self.theme.selection, width, height);
                }
                let fg = GColor::rgb(self.theme.fg[0], self.theme.fg[1], self.theme.fg[2]);
                let lb = shape_label(&mut self.font_system, label, self.font_size, self.cell_h, fg);
                panel_bufs.push((lb, px + self.cell_w, ry + line_h * 0.3, fg));
                let vcol = GColor::rgb(self.theme.ansi[3][0], self.theme.ansi[3][1], self.theme.ansi[3][2]);
                let vstr = format!("◀ {value} ▶");
                let vchars = vstr.chars().count() as f32;
                let vb = shape_label(&mut self.font_system, &vstr, self.font_size, self.cell_h, vcol);
                let vx = px + pw - self.cell_w - vchars * self.cell_w;
                panel_bufs.push((vb, vx, ry + line_h * 0.3, vcol));
            }
        }

        let rect_count = verts.len() as u32;
        let rect_vbuf = if verts.is_empty() {
            None
        } else {
            Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("term-rects"),
                contents: bytemuck::cast_slice(verts.as_slice()),
                usage: wgpu::BufferUsages::VERTEX,
            }))
        };

        // ---- 2) 文字：脏行才重排 ----
        if self.line_cache.len() != rows {
            self.line_cache = (0..rows).map(|_| None).collect();
        }
        for row in 0..rows {
            let cursor_col = if show_cursor && grid.cy == row {
                Some(grid.cx)
            } else {
                None
            };
            let h = hash_row(grid, row, cursor_col);
            let stale = match &self.line_cache[row] {
                Some(c) => c.hash != h,
                None => true,
            };
            if stale {
                let runs = build_row_runs(
                    &mut self.font_system,
                    grid,
                    row,
                    self.theme,
                    self.cell_w,
                    self.cell_h,
                    self.font_size,
                    term_left,
                    cursor_col,
                );
                self.line_cache[row] = Some(CachedLine { hash: h, runs });
            }
        }

        let (bw, bh) = (self.config.width as i32, self.config.height as i32);
        let mut text_areas: Vec<TextArea> = Vec::new();
        // 偏好面板打开时是模态：只画面板文字，终端/侧边栏文字让暗化层盖住。
        if !pref_open {
            // 侧边栏标签。
            let sw_i = sw as i32;
            for (buf, lx, ly, color) in &label_bufs {
                text_areas.push(TextArea {
                    buffer: buf,
                    left: *lx,
                    top: *ly,
                    scale: 1.0,
                    bounds: TextBounds { left: 0, top: 0, right: sw_i, bottom: bh },
                    default_color: *color,
                    custom_glyphs: &[],
                });
            }
            // 终端各行。
            for row in 0..rows {
                if let Some(line) = &self.line_cache[row] {
                    let top = term_top + row as f32 * self.cell_h;
                    for rb in &line.runs {
                        text_areas.push(TextArea {
                            buffer: &rb.buffer,
                            left: rb.left,
                            top,
                            scale: 1.0,
                            bounds: TextBounds {
                                left: term_left as i32,
                                top: term_top as i32,
                                right: bw,
                                bottom: bh,
                            },
                            default_color: rb.color,
                            custom_glyphs: &[],
                        });
                    }
                }
            }
        }
        // 偏好面板文字（始终在最上层）。
        for (buf, lx, ly, color) in &panel_bufs {
            text_areas.push(TextArea {
                buffer: buf,
                left: *lx,
                top: *ly,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: bw, bottom: bh },
                default_color: *color,
                custom_glyphs: &[],
            });
        }

        self.viewport.update(
            &self.queue,
            Resolution { width: self.config.width, height: self.config.height },
        );

        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            eprintln!("[term] glyphon prepare 失败: {e:?}");
            return;
        }

        // ---- 3) 上屏 ----
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                if let Ok(s) = self.instance.create_surface(self.window.clone()) {
                    self.surface = s;
                    self.surface.configure(&self.device, &self.config);
                }
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                eprintln!("[term] 交换链校验错误，跳过本帧。");
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("term-encoder") });

        let bg = self.theme.bg;
        let clear = wgpu::Color {
            r: bg[0] as f64 / 255.0,
            g: bg[1] as f64 / 255.0,
            b: bg[2] as f64 / 255.0,
            a: 1.0,
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("term-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if let Some(vbuf) = rect_vbuf.as_ref() {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..rect_count, 0..1);
            }

            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                eprintln!("[term] glyphon render 失败: {e:?}");
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        self.atlas.trim();
    }
}

/// 量一个等宽字形（'M'）的步进作为列宽；失败回退到 0.6×字号。
fn measure_cell_width(font_system: &mut FontSystem, font_size: f32, line_height: f32) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buf.set_size(font_system, Some(font_size * 4.0), Some(line_height * 2.0));
    buf.set_text(
        font_system,
        "M",
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs()
        .next()
        .map(|run| run.line_w)
        .filter(|w| *w > 0.0)
        .unwrap_or(font_size * 0.6)
}

/// 对一行可见内容求哈希（含光标 + 滚动回看位置体现在 visible_cell 上）。
fn hash_row(grid: &Grid, row: usize, cursor_col: Option<usize>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for col in 0..grid.cols {
        grid.visible_cell(col, row).hash(&mut hasher);
    }
    cursor_col.hash(&mut hasher);
    hasher.finish()
}

/// 把第 `row` 可见行切成「同 (前景,粗,斜) 的连续段」，每段 shape 成一个 Buffer。
/// 光标所在格按 reverse 取色；宽字符右半（wide_spacer）跳过（左半字形已占两列）。
#[allow(clippy::too_many_arguments)]
fn build_row_runs(
    font_system: &mut FontSystem,
    grid: &Grid,
    row: usize,
    theme: Theme,
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    pad: f32,
    cursor_col: Option<usize>,
) -> Vec<RunBuf> {
    let is_cursor = |col: usize| cursor_col == Some(col);
    let cols = grid.cols;
    let mut runs: Vec<RunBuf> = Vec::new();
    let mut col = 0;
    while col < cols {
        let cell0 = grid.visible_cell(col, row);
        if cell0.wide_spacer {
            col += 1;
            continue;
        }
        let cur = is_cursor(col);
        let (fg0, _) = cell0.effective_colors(&theme, cur);
        let (bold0, italic0) = (cell0.bold, cell0.italic);
        let wide0 = char_width(cell0.c) == 2;
        let start = col;
        let mut text = String::new();
        text.push(cell0.c);
        col += 1;
        if wide0 {
            // 宽字符单独成段并锚定到它的列：不靠 shaper 跨格排版，避免 CJK/emoji 列漂移。
            if col < cols && grid.visible_cell(col, row).wide_spacer {
                col += 1; // 跳过它的右半占位
            }
        } else if !cur {
            while col < cols && !is_cursor(col) {
                let cell = grid.visible_cell(col, row);
                if cell.wide_spacer {
                    col += 1;
                    continue;
                }
                if char_width(cell.c) == 2 {
                    break; // 遇到宽字符断段，让它自己锚定
                }
                let (fg, _) = cell.effective_colors(&theme, false);
                if fg != fg0 || cell.bold != bold0 || cell.italic != italic0 {
                    break;
                }
                text.push(cell.c);
                col += 1;
            }
        }
        if text.trim().is_empty() {
            continue;
        }
        let left = pad + start as f32 * cell_w;
        let color = GColor::rgb(fg0[0], fg0[1], fg0[2]);
        let mut buf = Buffer::new(font_system, Metrics::new(font_size, cell_h));
        let w = (col - start) as f32 * cell_w + 2.0 * cell_w;
        buf.set_size(font_system, Some(w), Some(cell_h * 2.0));
        let mut attrs = Attrs::new().family(Family::Monospace).color(color);
        if bold0 {
            attrs = attrs.weight(Weight::BOLD);
        }
        if italic0 {
            attrs = attrs.style(Style::Italic);
        }
        buf.set_text(font_system, &text, &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(font_system, false);
        runs.push(RunBuf { buffer: buf, left, color });
    }
    runs
}

/// shape 一段 tab 标题文字。
fn shape_label(
    font_system: &mut FontSystem,
    text: &str,
    font_size: f32,
    cell_h: f32,
    color: GColor,
) -> Buffer {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, cell_h));
    buf.set_size(font_system, Some(font_size * text.chars().count().max(1) as f32 * 2.0 + cell_h), Some(cell_h * 2.0));
    buf.set_text(
        font_system,
        text,
        &Attrs::new().family(Family::SansSerif).color(color),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(font_system, false);
    buf
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn darken(rgb: [u8; 3], f: f32) -> [u8; 3] {
    [
        (rgb[0] as f32 * f) as u8,
        (rgb[1] as f32 * f) as u8,
        (rgb[2] as f32 * f) as u8,
    ]
}

fn status_color(status: WinStatus, alerted: bool) -> [u8; 3] {
    if alerted {
        return [0xF3, 0x8B, 0xA8]; // 角标：粉红
    }
    match status {
        WinStatus::Idle => [0xA6, 0xE3, 0xA1],    // 绿
        WinStatus::Running => [0xF9, 0xE2, 0xAF], // 黄
        WinStatus::Failed => [0xF3, 0x8B, 0xA8],  // 红
    }
}

#[allow(clippy::too_many_arguments)]
fn push_rect(v: &mut Vec<RectVertex>, x0: f32, y0: f32, x1: f32, y1: f32, rgb: [u8; 3], w: f32, h: f32) {
    let color = [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0];
    let to_clip = |x: f32, y: f32| -> [f32; 2] { [x / w * 2.0 - 1.0, 1.0 - y / h * 2.0] };
    let p00 = to_clip(x0, y0);
    let p10 = to_clip(x1, y0);
    let p01 = to_clip(x0, y1);
    let p11 = to_clip(x1, y1);
    for pos in [p00, p10, p11, p00, p11, p01] {
        v.push(RectVertex { pos, color });
    }
}

fn build_rect_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rect-shader"),
        source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("rect-layout"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("rect-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<RectVertex>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[
                    wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                    wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x3 },
                ],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}
