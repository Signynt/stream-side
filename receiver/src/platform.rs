
// Use real proxy if not android
#[cfg(not(target_os = "android"))]
pub type AppProxy = Option<winit::event_loop::EventLoopProxy<crate::UserEvent>>;

// Empty placeholder
#[cfg(target_os = "android")]
pub type AppProxy = Option<()>; 