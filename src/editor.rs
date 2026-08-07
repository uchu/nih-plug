//! Traits for working with plugin editors.

use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
use std::any::Any;
use std::ffi::c_void;
use std::sync::Arc;

use crate::prelude::GuiContext;

/// An editor for a [`Plugin`][crate::prelude::Plugin].
pub trait Editor: Send {
    /// Create an instance of the plugin's editor and embed it in the parent window. As explained in
    /// [`Plugin::editor()`][crate::prelude::Plugin::editor()], you can then read the parameter
    /// values directly from your [`Params`][crate::prelude::Params] object, and modifying the
    /// values can be done using the functions on the [`ParamSetter`][crate::prelude::ParamSetter].
    /// When you change a parameter value that way it will be broadcasted to the host and also
    /// updated in your [`Params`][crate::prelude::Params] struct.
    ///
    /// This function should return a handle to the editor, which will be dropped when the editor
    /// gets closed. Implement the [`Drop`] trait on the returned handle if you need to explicitly
    /// handle the editor's closing behavior.
    ///
    /// If [`set_scale_factor()`][Self::set_scale_factor()] has been called, then any created
    /// windows should have their sizes multiplied by that factor.
    ///
    /// The wrapper guarantees that a previous handle has been dropped before this function is
    /// called again.
    //
    // TODO: Think of how this would work with the event loop. On Linux the wrapper must provide a
    //       timer using VST3's `IRunLoop` interface, but on Window and macOS the window would
    //       normally register its own timer. Right now we just ignore this because it would
    //       otherwise be basically impossible to have this still be GUI-framework agnostic. Any
    //       callback that deos involve actual GUI operations will still be spooled to the IRunLoop
    //       instance.
    // TODO: This function should return an `Option` instead. Right now window opening failures are
    //       always fatal. This would need to be fixed in baseview first.
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send>;

    /// Returns the (current) size of the editor in pixels as a `(width, height)` pair. This size
    /// must be reported in _logical pixels_, i.e. the size before being multiplied by the DPI
    /// scaling factor to get the actual physical screen pixels.
    fn size(&self) -> (u32, u32);

    /// Set the DPI scaling factor, if supported. The plugin APIs don't make any guarantees on when
    /// this is called, but for now just assume it will be the first function that gets called
    /// before creating the editor. If this is set, then any windows created by this editor should
    /// have their sizes multiplied by this scaling factor on Windows and Linux.
    ///
    /// Right now this is never called on macOS since DPI scaling is built into the operating system
    /// there.
    fn set_scale_factor(&self, factor: f32) -> bool;

    /// Called whenever a specific parameter's value has changed while the editor is open. You don't
    /// need to do anything with this, but this can be used to force a redraw when the host sends a
    /// new value for a parameter or when a parameter change sent to the host gets processed.
    fn param_value_changed(&self, id: &str, normalized_value: f32);

    /// Called whenever a specific parameter's monophonic modulation value has changed while the
    /// editor is open.
    fn param_modulation_changed(&self, id: &str, modulation_offset: f32);

    /// Called whenever one or more parameter values or modulations have changed while the editor is
    /// open. This may be called in place of [`param_value_changed()`][Self::param_value_changed()]
    /// when multiple parameter values hcange at the same time. For example, when a preset is
    /// loaded.
    fn param_values_changed(&self);

    // TODO: Reconsider adding a tick function here for the Linux `IRunLoop`. To keep this platform
    //       and API agnostic, add a way to ask the GuiContext if the wrapper already provides a
    //       tick function. If it does not, then the Editor implementation must handle this by
    //       itself. This would also need an associated `PREFERRED_FRAME_RATE` constant.

    /// Return `Some(..)` to let the host resize this editor. Defaults to `None`, which keeps the
    /// fixed-size behavior every editor had before this existed: the wrappers then tell the host
    /// the editor cannot be resized and reject any size but its own.
    fn resize_hints(&self) -> Option<ResizeHints> {
        None
    }

    /// Called when the host has resized the editor's parent window to `width` by `height` _logical
    /// pixels_, i.e. after dividing out the DPI scaling factor. Return `false` to reject the size.
    /// Only ever called when [`resize_hints()`][Self::resize_hints()] returns `Some(..)`, since the
    /// default implementation rejects everything.
    fn set_size(&self, _width: u32, _height: u32) -> bool {
        false
    }
}

/// Constraints a host must respect when resizing an editor, returned from
/// [`Editor::resize_hints()`]. All sizes are in logical pixels, like [`Editor::size()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeHints {
    pub min_width: u32,
    pub min_height: u32,
    /// Whether the editor must keep the aspect ratio it currently has.
    pub preserve_aspect_ratio: bool,
}

impl ResizeHints {
    /// The smallest integer aspect ratio that expresses `size`, as the `(width, height)` pair
    /// CLAP's `clap_gui_resize_hints` asks for.
    pub fn aspect_ratio(size: (u32, u32)) -> (u32, u32) {
        let (width, height) = (size.0.max(1), size.1.max(1));
        let divisor = gcd(width, height);

        (width / divisor, height / divisor)
    }

    /// Correct a size the host asked for into one this editor accepts: never smaller than the
    /// minimum, and on `current_size`'s aspect ratio when that has to be preserved. The result fits
    /// inside `requested` unless the minimum forces it larger.
    pub fn adjust_size(&self, current_size: (u32, u32), requested: (u32, u32)) -> (u32, u32) {
        let (width, height) = (requested.0.max(1), requested.1.max(1));
        if !self.preserve_aspect_ratio {
            return (width.max(self.min_width), height.max(self.min_height));
        }

        let (ratio_width, ratio_height) = Self::aspect_ratio(current_size);
        // Whole multiples of the reduced ratio land on it exactly, so a run of resizes can never
        // accumulate rounding error. A current size that barely reduces has no usable grid — its
        // steps would be as large as the window — so scale that directly instead.
        if ratio_width > width / 8 || ratio_height > height / 8 {
            let (current_width, current_height) =
                (current_size.0.max(1) as f64, current_size.1.max(1) as f64);
            let scale = (width as f64 / current_width)
                .min(height as f64 / current_height)
                .max(self.min_width as f64 / current_width)
                .max(self.min_height as f64 / current_height);

            return (
                (current_width * scale).round().max(1.0) as u32,
                (current_height * scale).round().max(1.0) as u32,
            );
        }

        let steps = (width / ratio_width)
            .min(height / ratio_height)
            .max(self.min_width.div_ceil(ratio_width))
            .max(self.min_height.div_ceil(ratio_height))
            .max(1);

        (steps * ratio_width, steps * ratio_height)
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }

    a.max(1)
}

/// A raw window handle for platform and GUI framework agnostic editors. This implements
/// [`HasRawWindowHandle`] so it can be used directly with GUI libraries that use the same
/// [`raw_window_handle`] version. If the library links against a different version of
/// `raw_window_handle`, then you'll need to wrap around this type and implement the trait yourself.
#[derive(Debug, Clone, Copy)]
pub enum ParentWindowHandle {
    /// The ID of the host's parent window. Used with X11.
    X11Window(u32),
    /// A handle to the host's parent window. Used only on macOS.
    AppKitNsView(*mut c_void),
    /// A handle to the host's parent window. Used only on Windows.
    Win32Hwnd(*mut c_void),
}

unsafe impl HasRawWindowHandle for ParentWindowHandle {
    fn raw_window_handle(&self) -> RawWindowHandle {
        match *self {
            ParentWindowHandle::X11Window(window) => {
                let mut handle = raw_window_handle::XcbWindowHandle::empty();
                handle.window = window;
                RawWindowHandle::Xcb(handle)
            }
            ParentWindowHandle::AppKitNsView(ns_view) => {
                let mut handle = raw_window_handle::AppKitWindowHandle::empty();
                handle.ns_view = ns_view;
                RawWindowHandle::AppKit(handle)
            }
            ParentWindowHandle::Win32Hwnd(hwnd) => {
                let mut handle = raw_window_handle::Win32WindowHandle::empty();
                handle.hwnd = hwnd;
                RawWindowHandle::Win32(handle)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINTS: ResizeHints = ResizeHints {
        min_width: 640,
        min_height: 360,
        preserve_aspect_ratio: true,
    };

    #[test]
    fn aspect_ratio_reduces() {
        assert_eq!(ResizeHints::aspect_ratio((1280, 720)), (16, 9));
        assert_eq!(ResizeHints::aspect_ratio((0, 0)), (1, 1));
    }

    #[test]
    fn adjusted_sizes_keep_the_ratio_exactly() {
        for requested in [(1000, 800), (1920, 1080), (700, 400), (2000, 500)] {
            let (width, height) = HINTS.adjust_size((1280, 720), requested);
            assert_eq!(ResizeHints::aspect_ratio((width, height)), (16, 9));
            assert!(width >= HINTS.min_width && height >= HINTS.min_height);
        }
    }

    #[test]
    fn adjusted_sizes_fit_inside_the_request() {
        let (width, height) = HINTS.adjust_size((1280, 720), (1000, 800));
        assert!(width <= 1000 && height <= 800);
    }

    #[test]
    fn the_minimum_wins_over_the_request() {
        assert_eq!(HINTS.adjust_size((1280, 720), (16, 9)), (640, 360));
    }

    #[test]
    fn a_coarse_ratio_still_scales() {
        // A size that does not reduce has no grid to snap to, so shrinking must still work.
        let current = (1009, 563);
        let (width, height) = HINTS.adjust_size(current, (800, 800));
        assert!(width < current.0 && width >= HINTS.min_width);
        assert!((width as f64 / height as f64 - current.0 as f64 / current.1 as f64).abs() < 0.01);
    }

    #[test]
    fn free_resizing_only_clamps() {
        let hints = ResizeHints {
            preserve_aspect_ratio: false,
            ..HINTS
        };
        assert_eq!(hints.adjust_size((1280, 720), (1000, 800)), (1000, 800));
        assert_eq!(hints.adjust_size((1280, 720), (100, 100)), (640, 360));
    }
}
