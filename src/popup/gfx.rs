//! Direct2D / DirectWrite resources and drawing primitives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_CAP_STYLE_ROUND,
    D2D1_DASH_STYLE_CUSTOM, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_LINE_JOIN_ROUND,
    D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Brush, ID2D1Factory, ID2D1HwndRenderTarget,
    ID2D1LinearGradientBrush, ID2D1PathGeometry, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{BOOL, HSTRING, Interface, w};
use windows_numerics::{Matrix3x2, Vector2};

use super::theme::{Theme, rgb};
use super::{rect, window_size};
use crate::model::Provider;
use crate::{logos, svgpath};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub(super) struct Fonts {
    pub(super) title: IDWriteTextFormat,
    pub(super) name: IDWriteTextFormat,
    pub(super) label: IDWriteTextFormat,
    pub(super) value: IDWriteTextFormat,
    pub(super) caption: IDWriteTextFormat,
    pub(super) pill: IDWriteTextFormat,
    pub(super) message: IDWriteTextFormat,
    pub(super) icon: IDWriteTextFormat,
    pub(super) icon_small: IDWriteTextFormat,
    pub(super) tiny: IDWriteTextFormat,
    pub(super) caption_wrap: IDWriteTextFormat,
}

pub(super) struct Gfx {
    // The single-threaded factory and its resources must stay on the creating UI thread.
    _ui_thread: PhantomData<Rc<()>>,
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle,
    dash: ID2D1StrokeStyle,
    /// Vector logos, in `Provider` order.
    logos: [ID2D1PathGeometry; 3],
    pub(super) fonts: Fonts,
    /// Global opacity (opening and view-change fades).
    pub(super) alpha: f32,
    /// Measured widths by (text format, text). Bounded: cleared when it grows past `WIDTHS_MAX`.
    widths: RefCell<HashMap<(usize, String), f32>>,
    /// Gradient brushes by stop colors. They belong to the render target and are dropped with it.
    gradients: RefCell<Vec<([u32; 8], ID2D1LinearGradientBrush)>>,
    /// Header gauge arc, keyed by its position and size.
    gauge: RefCell<Option<([u32; 3], ID2D1PathGeometry)>>,
}

const WIDTHS_MAX: usize = 256;

fn has_font(dwrite: &IDWriteFactory, family: &HSTRING) -> bool {
    // SAFETY: The factory stays alive; the HSTRING is terminated and all output pointers refer to local storage.
    unsafe {
        let mut collection = None;
        if dwrite.GetSystemFontCollection(&raw mut collection, false).is_err() {
            return false;
        }
        let Some(collection) = collection else { return false };
        let (mut index, mut exists) = (0u32, BOOL::default());
        collection.FindFamilyName(family, &raw mut index, &raw mut exists).is_ok() && exists.as_bool()
    }
}

fn color_bits(c: D2D1_COLOR_F) -> [u32; 4] {
    [c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()]
}

impl Gfx {
    pub(super) fn new(hwnd: HWND, dpi: f32) -> windows::core::Result<Self> {
        // SAFETY: Both factories return owned COM references. This constructor runs on the popup's UI thread,
        // and the PhantomData marker keeps this Gfx and its single-threaded factory on that thread.
        let (factory, dwrite) = unsafe {
            (
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
            )
        };
        let (target, brush) = Self::create_target(&factory, hwnd, dpi)?;
        // SAFETY: The factory is live on its creating thread; property temporaries and the dash slice last
        // through their synchronous CreateStrokeStyle calls.
        let (round, dash) = unsafe {
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            let dash = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    dashStyle: D2D1_DASH_STYLE_CUSTOM,
                    ..Default::default()
                },
                Some(&[0.1, 3.0]),
            )?;

            (round, dash)
        };

        let text_family = if has_font(&dwrite, &HSTRING::from("Segoe UI Variable Text")) {
            HSTRING::from("Segoe UI Variable Text")
        } else {
            HSTRING::from("Segoe UI")
        };
        let icon_family = if has_font(&dwrite, &HSTRING::from("Segoe Fluent Icons")) {
            HSTRING::from("Segoe Fluent Icons")
        } else {
            HSTRING::from("Segoe MDL2 Assets")
        };
        let make = |family: &HSTRING, weight, size, wrap| Self::text_format(&dwrite, family, weight, size, wrap);
        let fonts = Fonts {
            title: make(&text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 15.0, false)?,
            name: make(&text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0, false)?,
            label: make(&text_family, DWRITE_FONT_WEIGHT_NORMAL, 13.0, false)?,
            value: make(&text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0, false)?,
            caption: make(&text_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0, false)?,
            pill: make(&text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 11.0, false)?,
            message: make(&text_family, DWRITE_FONT_WEIGHT_NORMAL, 13.0, true)?,
            icon: make(&icon_family, DWRITE_FONT_WEIGHT_NORMAL, 14.0, false)?,
            icon_small: make(&icon_family, DWRITE_FONT_WEIGHT_NORMAL, 11.0, false)?,
            tiny: make(&text_family, DWRITE_FONT_WEIGHT_NORMAL, 10.0, false)?,
            caption_wrap: make(&text_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0, true)?,
        };
        let logos = [
            svgpath::geometry(&factory, logos::CLAUDE, false)?,
            svgpath::geometry(&factory, logos::CODEX, true)?,
            svgpath::geometry(&factory, logos::AMP, false)?,
        ];
        Ok(Gfx {
            _ui_thread: PhantomData,
            factory,
            dwrite,
            target,
            brush,
            round,
            dash,
            logos,
            fonts,
            alpha: 1.0,
            widths: RefCell::default(),
            gradients: RefCell::default(),
            gauge: RefCell::default(),
        })
    }

    fn text_format(
        dwrite: &IDWriteFactory,
        family: &HSTRING,
        weight: DWRITE_FONT_WEIGHT,
        size: f32,
        wrap: bool,
    ) -> windows::core::Result<IDWriteTextFormat> {
        // SAFETY: The factory and resulting format stay alive on the UI thread. Font and locale strings are
        // terminated; the trimming structure and sign remain alive through SetTrimming.
        unsafe {
            let f = dwrite.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("fr-fr"),
            )?;
            // Single lines are centered vertically in their rect; wrapped text flows from the top.
            if wrap {
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            } else {
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let trimming =
                    DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                let sign = dwrite.CreateEllipsisTrimmingSign(&f)?;
                f.SetTrimming(&raw const trimming, &sign)?;
            }
            Ok(f)
        }
    }

    fn create_target(
        factory: &ID2D1Factory,
        hwnd: HWND,
        dpi: f32,
    ) -> windows::core::Result<(ID2D1HwndRenderTarget, ID2D1SolidColorBrush)> {
        // SAFETY: The factory and HWND remain live on the window thread; both property structures are initialized and
        // read only during creation.
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // Software rendering: no GPU driver gets loaded into the process (measured: ~8 MB allocated with
                // the popup open, against ~60 MB on the GPU). Plenty fast for 360x400 DIP.
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: window_size(hwnd),
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = factory.CreateHwndRenderTarget(&raw const props, &raw const hwnd_props)?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = target.CreateSolidColorBrush(&rgb(0, 1.0), None)?;
            Ok((target, brush))
        }
    }

    pub(super) fn begin_draw(&self, transform: Matrix3x2) {
        // SAFETY: The owned target is used on its creating UI thread; Clear reads a color temporary valid for the call.
        unsafe {
            self.target.BeginDraw();
            self.target.Clear(Some(&rgb(0, 0.0)));
        }
        self.transform(transform);
    }

    pub(super) fn end_draw(&mut self, hwnd: HWND, dpi: f32) {
        self.transform(Matrix3x2::identity());
        // SAFETY: The live target is used on its creating UI thread; render pairs this with begin_draw.
        let result = unsafe { self.target.EndDraw(None, None) };
        // Device lost (sleep, display change): recreate the target and the brushes bound to it.
        if result.is_err_and(|e| e.code() == D2DERR_RECREATE_TARGET)
            && let Ok((target, brush)) = Self::create_target(&self.factory, hwnd, dpi)
        {
            self.target = target;
            self.brush = brush;
            self.gradients.get_mut().clear();
        }
    }

    pub(super) fn resize(&self, size: D2D_SIZE_U) {
        // SAFETY: The target stays alive on the UI thread; size is readable for the synchronous Resize call.
        unsafe {
            let current = self.target.GetPixelSize();
            if (current.width, current.height) != (size.width, size.height) {
                let _ = self.target.Resize(&raw const size);
            }
        }
    }

    pub(super) fn set_dpi(&self, dpi: f32) {
        // SAFETY: The target is owned by this Gfx and accessed on the UI thread that created it.
        unsafe { self.target.SetDpi(dpi, dpi) };
    }

    /// Keeps a clip active until the returned guard is dropped.
    pub(super) fn clip(&self, bounds: D2D_RECT_F) -> Clip<'_> {
        // SAFETY: The target is live on the UI thread and bounds lasts through the call. The guard borrows
        // this Gfx so the target cannot be replaced before its matching PopAxisAlignedClip.
        unsafe { self.target.PushAxisAlignedClip(&raw const bounds, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        Clip { gfx: self }
    }

    pub(super) fn transform(&self, m: Matrix3x2) {
        // SAFETY: The owned target is live on its creating UI thread and m is readable until SetTransform returns.
        unsafe { self.target.SetTransform(&raw const m) };
    }

    fn color(&self, c: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
        // SAFETY: The owned brush is live on its creating UI thread; SetColor copies the temporary color during the
        // call.
        unsafe { self.brush.SetColor(&D2D1_COLOR_F { a: c.a * self.alpha, ..c }) };
        &self.brush
    }

    /// Linear gradient from `from` at `start` to `to` at `end`, with the global opacity applied.
    fn gradient(
        &self,
        from: D2D1_COLOR_F,
        to: D2D1_COLOR_F,
        start: Vector2,
        end: Vector2,
    ) -> Option<ID2D1LinearGradientBrush> {
        let [a, b] = [color_bits(from), color_bits(to)];
        let key = [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]];
        let cached = self.gradients.borrow().iter().find(|(k, _)| *k == key).map(|(_, brush)| brush.clone());
        // SAFETY: The target and cached brushes are live on the UI thread. New stops and properties outlive their
        // calls; new brushes use this target.
        unsafe {
            let brush = if let Some(brush) = cached {
                brush
            } else {
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: from },
                    D2D1_GRADIENT_STOP { position: 1.0, color: to },
                ];
                let collection =
                    self.target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).ok()?;
                let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: start, endPoint: end };
                let brush = self.target.CreateLinearGradientBrush(&raw const props, None, &collection).ok()?;
                self.gradients.borrow_mut().push((key, brush.clone()));
                brush
            };
            brush.SetStartPoint(start);
            brush.SetEndPoint(end);
            brush.SetOpacity(self.alpha);
            Some(brush)
        }
    }

    pub(super) fn fill(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        self.fill_brush(r, radius, self.color(c));
    }

    fn fill_brush(&self, r: D2D_RECT_F, radius: f32, brush: &ID2D1Brush) {
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        // SAFETY: The rectangle is initialized and readable for the call; the live brush belongs to this target on the
        // UI thread.
        unsafe { self.target.FillRoundedRectangle(&raw const rr, brush) };
    }

    pub(super) fn stroke(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        let r = D2D_RECT_F { left: r.left + 0.5, top: r.top + 0.5, right: r.right - 0.5, bottom: r.bottom - 0.5 };
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        // SAFETY: The rectangle is initialized and readable for the call; the live brush belongs to this target on the
        // UI thread.
        unsafe { self.target.DrawRoundedRectangle(&raw const rr, self.color(c), 1.0, None) };
    }

    pub(super) fn line(&self, a: (f32, f32), b: (f32, f32), width: f32, c: D2D1_COLOR_F) {
        self.draw_line(a, b, width, c, &self.round);
    }

    pub(super) fn dashed(&self, a: (f32, f32), b: (f32, f32), width: f32, c: D2D1_COLOR_F) {
        self.draw_line(a, b, width, c, &self.dash);
    }

    fn draw_line(&self, a: (f32, f32), b: (f32, f32), width: f32, c: D2D1_COLOR_F, style: &ID2D1StrokeStyle) {
        // SAFETY: The target, its brush and the factory's stroke style are live on their creating UI thread.
        unsafe {
            self.target.DrawLine(Vector2 { X: a.0, Y: a.1 }, Vector2 { X: b.0, Y: b.1 }, self.color(c), width, style);
        }
    }

    /// Fills `geometry` scaled by `s` and placed at (`x`, `y`).
    fn path(&self, geometry: &ID2D1PathGeometry, x: f32, y: f32, s: f32, c: D2D1_COLOR_F) {
        // SAFETY: The geometry, target and brush stay alive on the UI thread; transform pointers refer to initialized
        // local matrices.
        unsafe {
            let mut current = Matrix3x2::identity();
            self.target.GetTransform(&raw mut current);
            self.target.SetTransform(&(Matrix3x2::scale(s, s) * Matrix3x2::translation(x, y) * current));
            self.target.FillGeometry(geometry, self.color(c), None);
            self.target.SetTransform(&raw const current);
        }
    }

    /// Provider logo in a 20x20 DIP square.
    pub(super) fn provider_logo(&self, th: &Theme, p: Provider, x: f32, y: f32) {
        let color = match p {
            Provider::Claude => rgb(0xD97757, 1.0),
            Provider::Codex => th.text1,
            Provider::Amp => {
                // Amp app icon: dark green square, cream wordmark.
                let badge = rect(x, y, 20.0, 20.0);
                self.fill_vertical_gradient(badge, 5.0, rgb(0x2C3A2A, 1.0), rgb(0x061615, 1.0));
                self.stroke(badge, 5.0, rgb(0xDFDFC1, if th.dark { 0.14 } else { 0.0 }));
                let (vw, _) = logos::AMP_VIEWBOX;
                let s = 15.5 / vw;
                // The wordmark spans y 20..143 of the viewBox: center that band.
                self.path(&self.logos[2], x + 2.25, y + 10.0 - 81.5 * s, s, rgb(0xDFDFC1, 1.0));
                return;
            }
        };
        // The Claude and Codex paths use a 24x24 viewBox.
        self.path(&self.logos[p as usize], x, y, 20.0 / 24.0, color);
    }

    fn fill_vertical_gradient(&self, r: D2D_RECT_F, radius: f32, top: D2D1_COLOR_F, bottom: D2D1_COLOR_F) {
        let Some(brush) = self.gradient(top, bottom, Vector2 { X: 0.0, Y: r.top }, Vector2 { X: 0.0, Y: r.bottom })
        else {
            return;
        };
        self.fill_brush(r, radius, &brush);
    }

    pub(super) fn disc(&self, (x, y): (f32, f32), r: f32, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r };
        // SAFETY: The ellipse lasts through the call and the live brush belongs to this target on the UI thread.
        unsafe { self.target.FillEllipse(&raw const e, self.color(c)) };
    }

    /// Area under a curve, with a vertical gradient fading to transparent.
    pub(super) fn area(&self, points: &[(f32, f32)], top: f32, bottom: f32, c: D2D1_COLOR_F) {
        let (Some(first), Some(last)) = (points.first(), points.last()) else { return };
        let v = |(x, y): (f32, f32)| Vector2 { X: x, Y: y };
        let Some(brush) = self.gradient(
            D2D1_COLOR_F { a: 0.34, ..c },
            D2D1_COLOR_F { a: 0.02, ..c },
            v((0.0, top)),
            v((0.0, bottom)),
        ) else {
            return;
        };
        // SAFETY: The factory, geometry and sink stay alive on the UI thread. The figure is ended and sink closed
        // before drawing with this target's brush.
        unsafe {
            let Ok(path) = self.factory.CreatePathGeometry() else { return };
            let Ok(sink) = path.Open() else { return };
            sink.BeginFigure(v((first.0, bottom)), D2D1_FIGURE_BEGIN_FILLED);
            for &p in points {
                sink.AddLine(v(p));
            }
            sink.AddLine(v((last.0, bottom)));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            if sink.Close().is_err() {
                return;
            }
            self.target.FillGeometry(&path, &brush, None);
        }
    }

    pub(super) fn text(
        &self,
        s: &str,
        format: &IDWriteTextFormat,
        r: D2D_RECT_F,
        c: D2D1_COLOR_F,
        align: DWRITE_TEXT_ALIGNMENT,
    ) {
        // SAFETY: The format and target stay alive on the UI thread; DrawText reads the UTF-16 slice and rectangle only
        // during the call.
        unsafe {
            let _ = format.SetTextAlignment(align);
            self.target.DrawText(
                &wide(s),
                format,
                &raw const r,
                self.color(c),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Natural width of `s`. Cached: measuring builds a text layout, and the same labels are measured on every
    /// animation frame.
    pub(super) fn text_width(&self, s: &str, format: &IDWriteTextFormat) -> f32 {
        let key = (format.as_raw() as usize, s.to_owned());
        if let Some(&width) = self.widths.borrow().get(&key) {
            return width;
        }
        // SAFETY: The factory and format stay alive on the UI thread; the UTF-16 slice outlives CreateTextLayout and
        // metrics points to writable local storage.
        let width = unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let Ok(layout) = self.dwrite.CreateTextLayout(&wide(s), format, 1000.0, 100.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&raw mut m);
            m.widthIncludingTrailingWhitespace
        };
        let mut widths = self.widths.borrow_mut();
        if widths.len() >= WIDTHS_MAX {
            widths.clear();
        }
        widths.insert(key, width);
        width
    }

    /// The app icon gauge, with its gradient (same drawing as `art.rs`, as vectors).
    pub(super) fn gauge(&self, x: f32, y: f32, size: f32) {
        let s = size / 16.0;
        let p = |px: f32, py: f32| Vector2 { X: x + px * s, Y: y + py * s };
        let polar = |deg: f32, r: f32| {
            let a = deg.to_radians();
            p(8.0 + r * a.cos(), 9.4 - r * a.sin())
        };
        let key = [x.to_bits(), y.to_bits(), size.to_bits()];
        let cached = self.gauge.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, path)| path.clone());
        let Some(path) = cached.or_else(|| {
            let path = self.gauge_arc(polar(210.0, 6.0), polar(-30.0, 6.0), 6.0 * s)?;
            *self.gauge.borrow_mut() = Some((key, path.clone()));
            Some(path)
        }) else {
            return;
        };
        let Some(brush) = self.gradient(rgb(0xFF8A5B, 1.0), rgb(0x8B6BFF, 1.0), p(1.0, 2.0), p(15.0, 14.0)) else {
            return;
        };
        // SAFETY: The path and stroke style belong to this factory, the brush to this target; all stay alive on the UI
        // thread and hub lasts through FillEllipse.
        unsafe {
            self.target.DrawGeometry(&path, &brush, 1.9 * s, &self.round);
            self.target.DrawLine(p(8.0, 9.4), polar(42.0, 3.9), &brush, crate::art::NEEDLE_W * s, &self.round);
            let r = crate::art::HUB_R * s;
            let hub = D2D1_ELLIPSE { point: p(8.0, 9.4), radiusX: r, radiusY: r };
            self.target.FillEllipse(&raw const hub, &brush);
        }
    }

    fn gauge_arc(&self, from: Vector2, to: Vector2, radius: f32) -> Option<ID2D1PathGeometry> {
        // SAFETY: The factory, path and sink stay alive on the UI thread; AddArc reads a temporary valid through the
        // call, then the figure and sink are closed.
        unsafe {
            let path = self.factory.CreatePathGeometry().ok()?;
            let sink = path.Open().ok()?;
            sink.BeginFigure(from, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: to,
                size: D2D_SIZE_F { width: radius, height: radius },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_LARGE,
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close().ok()?;
            Some(path)
        }
    }
}

pub(super) struct Clip<'a> {
    gfx: &'a Gfx,
}

impl Drop for Clip<'_> {
    fn drop(&mut self) {
        // SAFETY: clip pushed one clip on this target, and the borrow keeps it alive until this matching pop.
        unsafe { self.gfx.target.PopAxisAlignedClip() };
    }
}
