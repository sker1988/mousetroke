//! 轨迹覆盖层：全虚拟屏、透明、点击穿透的分层窗口 + 软件光栅（预乘 ARGB DIB）。
//!
//! **性能铁律（钩子回调内零阻塞）**：
//! - 会话开始不做任何系统调用（无清屏、无提交），首次绘制才显示窗口
//! - 提交一律走 `UpdateLayeredWindowIndirect` 的**脏矩形**路径
//! - 淡出动画由 `WM_TIMER` 驱动（消息循环泵送，不在回调里 sleep）
//! - 只对"内容包围盒"做像素操作，绝不整屏遍历
//!
//! 以后若换 Direct2D 渲染，架构不变，只替换渲染后端。

use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetTextExtentPoint32W, SelectObject, TextOutW,
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BLENDFUNCTION, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HGDIOBJ, HDC, HBITMAP,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, KillTimer, RegisterClassW, SetPropW, SetTimer,
    SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_SHOWWINDOW, UpdateLayeredWindow, UpdateLayeredWindowIndirect, CS_HREDRAW,
    CS_VREDRAW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, UPDATELAYEREDWINDOWINFO,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

const CLASS_NAME: &str = "MousetrokeOverlay";
const FADE_TIMER_ID: usize = 1;
const FADE_FRAMES: u8 = 5;
const FADE_INTERVAL_MS: u32 = 16;

/// "#RRGGBBAA"（旧格式，如 #00000030 = 黑色 19% 透明）→ (a, r, g, b)。
/// 6 位 "#RRGGBB" 视为不透明。
pub fn parse_argb(s: &str) -> (u8, u8, u8, u8) {
    let hex = s.trim_start_matches('#');
    let v = u64::from_str_radix(hex, 16).unwrap_or(0x0000_0030);
    match hex.len() {
        8 => (
            (v & 0xff) as u8,
            ((v >> 24) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
        ),
        6 => (0xff, ((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8),
        _ => (0x30, 0, 0, 0),
    }
}

/// 覆盖层。所有方法须在引擎线程（安装钩子的线程）调用。
pub struct Overlay {
    hwnd: HWND,
    mem_dc: HDC,
    _dib: HBITMAP, // 选入 mem_dc 的位图
    pixels: *mut u32,
    width: i32,
    height: i32,
    virtual_height: i32,
    vx: i32,
    vy: i32,
    visible: bool,
    /// 上一个原始输入点（平滑曲线的控制点）。
    prev: Option<(f64, f64)>,
    /// 已画到的位置（上一段的中点）。
    mid: Option<(f64, f64)>,
    half_width: f64,
    color: (u8, u8, u8, u8),
    last_present: Option<std::time::Instant>,
    ulw_warned: bool,
    /// 被跳过（节流）的待提交区域并集。
    pending_dirty: Option<RECT>,
    /// 本次会话绘制内容的包围盒（淡出/清屏只处理此区域）。
    content_bounds: Option<RECT>,
    fade_frames: u8,
    // 标签（手势名）渲染的暂存资源与位置
    label_rect: Option<(i32, i32, i32, i32)>, // 本地坐标
    /// 标签层（预乘 ARGB，按 label_rect 行优先）与其下方原有内容。
    /// 清除标签时还原下方内容，不会把轨迹擦掉一块。淡出时丢弃（整体一起淡出）。
    label_layer: Vec<u32>,
    label_under: Vec<u32>,
    scratch_dc: HDC,
    scratch_ptr: *mut u32,
    _scratch_dib: HBITMAP,
    scratch_width: i32,
    scratch_height: i32,
    font: windows::Win32::Graphics::Gdi::HFONT,
    font_size: u32,
    /// 标签字体创建时的语言标签（语言切换后丢弃重建）。
    font_tag: &'static str,
}

unsafe impl Send for Overlay {}

/// 显示器布局变化时会换新覆盖层，旧的窗口与 GDI 资源必须释放。
impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self._dib.0));
            if !self.scratch_dc.is_invalid() {
                let _ = DeleteDC(self.scratch_dc);
            }
            if !self._scratch_dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self._scratch_dib.0));
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

/// 系统当前的虚拟屏 (x, y, w, h)；覆盖层按它创建，也用来发现过期的覆盖层。
pub fn system_virtual_rect() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

impl Overlay {
    /// 创建（窗口隐藏且非置顶状态，缓冲区全透明）。
    pub fn new(color_normal: &str, width_px: f64) -> Option<Self> {
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let mut classname: Vec<u16> = CLASS_NAME.encode_utf16().chain([0]).collect();

            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wndproc),
                hInstance: hinst.into(),
                lpszClassName: windows::core::PCWSTR(classname.as_mut_ptr()),
                ..Default::default()
            };
            let atom = RegisterClassW(&wc);
            // 显示器布局变化时会再次创建：类已注册不算失败。
            // 注意 Error::code() 是 HRESULT，不能直接和 Win32 错误码 1410 比。
            if atom == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return None;
            }

            let (vx, vy, vw, vh) = system_virtual_rect();
            // 窗口与位图高度减 1 像素，从几何判定上彻底跳过 Windows Shell 的全屏（Rude Window）检测
            let win_h = (vh - 1).max(1);
            crate::log!("覆盖层: ({vx},{vy}) {vw}×{win_h} (虚拟屏高度 {vh})");

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::PCWSTR(classname.as_mut_ptr()),
                None,
                WS_POPUP,
                vx,
                vy,
                vw,
                win_h,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .ok()?;

            // 标记为非侵入式全屏窗口，作为几何免疫之外的双保险
            let non_rude: Vec<u16> = "NonRudeHWND".encode_utf16().chain([0]).collect();
            let _ = SetPropW(
                hwnd,
                PCWSTR(non_rude.as_ptr()),
                Some(HANDLE(1usize as *mut core::ffi::c_void)),
            );

            // 32bpp 顶向下 DIB，预乘 ARGB
            let mem_dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: vw,
                    biHeight: -win_h, // 顶向下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|e| crate::log!("DIB 创建失败: {e}"))
                .ok()?;
            if bits.is_null() {
                return None;
            }
            // DIB 段内存由系统按需零填充；不再整屏写零，避免启动即提交整块工作集。
            let _old = SelectObject(mem_dc, HGDIOBJ(dib.0));

            Some(Self {
                hwnd,
                mem_dc,
                _dib: dib,
                pixels: bits as *mut u32,
                width: vw,
                height: win_h,
                virtual_height: vh,
                vx,
                vy,
                visible: false,
                prev: None,
                mid: None,
                half_width: half_width(width_px),
                color: parse_argb(color_normal),
                last_present: None,
                ulw_warned: false,
                pending_dirty: None,
                content_bounds: None,
                fade_frames: 0,
                label_rect: None,
                label_layer: Vec::new(),
                label_under: Vec::new(),
                scratch_dc: HDC(std::ptr::null_mut()),
                scratch_ptr: std::ptr::null_mut(),
                _scratch_dib: HBITMAP(std::ptr::null_mut()),
                scratch_width: 0,
                scratch_height: 0,
                font: windows::Win32::Graphics::Gdi::HFONT::default(),
                font_size: 24,
                font_tag: "",
            })
        }
    }

    pub fn set_color(&mut self, color: &str) {
        self.color = parse_argb(color);
    }

    pub fn set_width(&mut self, width_px: f64) {
        self.half_width = half_width(width_px);
    }

    pub fn set_label_font_size(&mut self, size_px: u32) {
        let size = size_px.clamp(16, 96);
        if self.font_size != size {
            self.font_size = size;
            // 下次绘制时在现有 DC 中换入新字体，再安全释放旧字体。
            self.font_tag = "";
        }
    }

    /// 手势会话开始：重置状态并**强制提交清掉上次残留**（区域级，代价小）。
    /// 若窗口当前处于隐藏态则零系统调用。
    pub fn start_session(&mut self, x: i32, y: i32) {
        self.stop_fade();
        // 先记下上次的内容区（轨迹 ∪ 标签），清零后再提交——不能先 take 走 content_bounds，
        // 否则 zero_content 拿不到区域，淡出被打断的半透明轨迹会永远留在屏幕上
        let label = self.label_rect.map(|(l, t, r, b)| RECT { left: l, top: t, right: r, bottom: b });
        let stale = match (self.content_bounds, label) {
            (Some(c), Some(l)) => Some(union_rect(&c, &l)),
            (c, l) => c.or(l),
        };
        self.zero_content();
        if let Some(b) = stale {
            // 上次会话的内容可能仍在窗口表面上（淡出未跑完/中断），强制清一次
            if self.visible {
                self.present_dirty(&b, true);
            }
        }
        if self.visible {
            // 淡出被打断时窗口仍在可见态，重新确保置顶
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
                );
            }
        }
        self.prev = Some(self.to_local(x, y));
        self.mid = None;
    }

    /// 手势中：画到 (x, y) 并脏矩形提交（首次绘制才显示窗口）。
    ///
    /// 中点平滑：从上一段中点到本段中点画二次贝塞尔（控制点为上一个输入点），
    /// 再叠加轻度指数平滑，鼠标事件的整数抖动和折线拐角都会被抹平。
    pub fn line_to(&mut self, x: i32, y: i32) {
        let raw = self.to_local(x, y);
        let Some(c) = self.prev else {
            return;
        };
        // 轻度指数平滑：滤掉逐像素抖动，滞后约半个事件
        let p = (c.0 + (raw.0 - c.0) * 0.55, c.1 + (raw.1 - c.1) * 0.55);
        self.prev = Some(p);
        let from = self.mid.unwrap_or(c);
        let to = ((c.0 + p.0) / 2.0, (c.1 + p.1) / 2.0);
        self.mid = Some(to);
        let len = (c.0 - from.0).hypot(c.1 - from.1) + (to.0 - c.0).hypot(to.1 - c.1);
        let n = (len / 3.0).ceil().clamp(1.0, 48.0) as i32;
        let mut dirty = RECT { left: i32::MAX, top: i32::MAX, right: i32::MIN, bottom: i32::MIN };
        let mut a = from;
        for i in 1..=n {
            let t = i as f64 / n as f64;
            let u = 1.0 - t;
            let b = (
                u * u * from.0 + 2.0 * u * t * c.0 + t * t * to.0,
                u * u * from.1 + 2.0 * u * t * c.1 + t * t * to.1,
            );
            dirty = union_rect(&dirty, &self.stroke(a, b));
            a = b;
        }
        if dirty.left < dirty.right {
            self.union_bounds(&dirty);
            self.present_dirty(&dirty, false);
        }
    }

    /// 手势结束：清内容区域并提交全透明帧，随后退顶隐藏。
    pub fn finish(&mut self) {
        self.prev = None;
        self.mid = None;
        self.stop_fade();
        let bounds = self.content_bounds;
        self.zero_content();
        if let Some(b) = bounds {
            if self.visible {
                self.present_dirty(&b, true);
            }
        }
        self.hide();
    }

    /// 手势结束：淡出（**非阻塞**——WM_TIMER 驱动，回调立即返回）。
    pub fn fade_out(&mut self) {
        self.prev = None;
        self.mid = None;
        // 标签与轨迹一起淡出：不再单独还原
        self.label_layer.clear();
        self.label_under.clear();
        if self.content_bounds.is_none() {
            self.hide();
            return; // 没画过东西
        }
        if !self.visible {
            self.finish();
            return;
        }
        self.fade_frames = FADE_FRAMES;
        unsafe {
            SetTimer(Some(self.hwnd), FADE_TIMER_ID, FADE_INTERVAL_MS, None);
        }
    }

    /// 淡出动画的一帧（由 overlay wndproc 的 WM_TIMER 调用）。
    pub fn tick_fade(&mut self) {
        if self.fade_frames == 0 {
            return;
        }
        if let Some(b) = self.content_bounds {
            let (l, t, r, bo) = (b.left.max(0), b.top.max(0), b.right.min(self.width), b.bottom.min(self.height));
            if r > l && bo > t {
                unsafe {
                    for y in t..bo {
                        let row = self.pixels.add(y as usize * self.width as usize);
                        for x in l..r {
                            let p = row.add(x as usize);
                            let v = *p;
                            *p = ((((v >> 24) & 0xff) * 13 / 18) << 24)
                                | ((((v >> 16) & 0xff) * 13 / 18) << 16)
                                | ((((v >> 8) & 0xff) * 13 / 18) << 8)
                                | ((v & 0xff) * 13 / 18);
                        }
                    }
                }
            }
            let mut rect = b;
            // 标签区域一并纳入
            if let Some((lx, ly, lr, lb)) = self.label_rect {
                rect.left = rect.left.min(lx);
                rect.top = rect.top.min(ly);
                rect.right = rect.right.max(lr);
                rect.bottom = rect.bottom.max(lb);
            }
            self.present_dirty(&rect, true);
        }
        self.fade_frames -= 1;
        if self.fade_frames == 0 {
            self.stop_fade();
            let bounds = self.content_bounds;
            self.zero_content();
            if let Some(b) = bounds {
                if self.visible {
                    self.present_dirty(&b, true);
                }
            }
            self.hide();
        }
    }

    fn stop_fade(&mut self) {
        if self.fade_frames > 0 {
            self.fade_frames = 0;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), FADE_TIMER_ID);
            }
        }
    }

    /// 清空内容包围盒区域（含标签），像素级、只碰画过的部分。
    fn zero_content(&mut self) {
        let mut bounds = self.content_bounds.take();
        if let Some((lx, ly, lr, lb)) = self.label_rect.take() {
            bounds = Some(match bounds {
                Some(b) => RECT {
                    left: b.left.min(lx),
                    top: b.top.min(ly),
                    right: b.right.max(lr),
                    bottom: b.bottom.max(lb),
                },
                None => RECT { left: lx, top: ly, right: lr, bottom: lb },
            });
        }
        if let Some(b) = bounds {
            let (l, t, r, bo) = (b.left.max(0), b.top.max(0), b.right.min(self.width), b.bottom.min(self.height));
            if r > l && bo > t {
                unsafe {
                    for y in t..bo {
                        let row = self.pixels.add(y as usize * self.width as usize);
                        std::ptr::write_bytes(row.add(l as usize), 0, (r - l) as usize * 4);
                    }
                }
            }
        }
        self.content_bounds = None;
        self.label_rect = None;
        self.label_layer.clear();
        self.label_under.clear();
        self.pending_dirty = None;
    }

    /// 提交新帧后升至置顶并显示，避免空闲期常驻 Topmost 队列。
    fn show(&mut self) {
        if !self.visible {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
            }
            self.visible = true;
        }
    }

    /// 退出置顶并隐藏，彻底从 Topmost 队列与
