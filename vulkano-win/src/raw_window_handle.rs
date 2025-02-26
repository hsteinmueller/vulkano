#[cfg(target_os = "ios")]
use crate::get_metal_layer_ios;
#[cfg(target_os = "macos")]
use crate::get_metal_layer_macos;
use raw_window_handle::{
    HasRawDisplayHandle, HasRawWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use std::sync::Arc;
use vulkano::{
    instance::Instance,
    swapchain::{Surface, SurfaceCreationError},
};

/// Creates a vulkan surface from a generic window
/// which implements HasRawWindowHandle and thus can reveal the os-dependent handle.
pub fn create_surface_from_handle<W>(
    window: W,
    instance: Arc<Instance>,
) -> Result<Arc<Surface<W>>, SurfaceCreationError>
where
    W: HasRawWindowHandle + HasRawDisplayHandle,
{
    unsafe {
        match window.raw_window_handle() {
            RawWindowHandle::AndroidNdk(h) => {
                Surface::from_android(instance, h.a_native_window, window)
            }
            RawWindowHandle::UiKit(_h) => {
                #[cfg(target_os = "ios")]
                {
                    // Ensure the layer is CAMetalLayer
                    let layer = get_metal_layer_ios(_h.ui_view);
                    Surface::from_ios(instance, layer, window)
                }
                #[cfg(not(target_os = "ios"))]
                {
                    panic!("UiKit handle should only be used when target_os == 'ios'");
                }
            }
            RawWindowHandle::AppKit(_h) => {
                #[cfg(target_os = "macos")]
                {
                    // Ensure the layer is CAMetalLayer
                    let layer = get_metal_layer_macos(_h.ns_view);
                    Surface::from_mac_os(instance, layer as *const (), window)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    panic!("AppKit handle should only be used when target_os == 'macos'");
                }
            }
            RawWindowHandle::Wayland(h) => {
                let d = match window.raw_display_handle() {
                    RawDisplayHandle::Wayland(d) => d,
                    _ => panic!("Invalid RawDisplayHandle"),
                };
                Surface::from_wayland(instance, d.display, h.surface, window)
            }
            RawWindowHandle::Win32(h) => Surface::from_win32(instance, h.hinstance, h.hwnd, window),
            RawWindowHandle::Xcb(h) => {
                let d = match window.raw_display_handle() {
                    RawDisplayHandle::Xcb(d) => d,
                    _ => panic!("Invalid RawDisplayHandle"),
                };
                Surface::from_xcb(instance, d.connection, h.window, window)
            }
            RawWindowHandle::Xlib(h) => {
                let d = match window.raw_display_handle() {
                    RawDisplayHandle::Xlib(d) => d,
                    _ => panic!("Invalid RawDisplayHandle"),
                };
                Surface::from_xlib(instance, d.display, h.window, window)
            }
            RawWindowHandle::Web(_) => unimplemented!(),
            _ => unimplemented!(),
        }
    }
}
