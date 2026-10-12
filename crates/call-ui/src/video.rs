//! 接收视频帧 → slint 纹理: GStreamer 流线程回调里拷帧进
//! SharedPixelBuffer, invoke_from_event_loop 把 Image 刷到 UI.
//! 25fps 级别的 invoke 开销可接受, 不做帧丢弃节流 (YAGNI).

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// 构造视频帧 tap: 每帧拷进 slint::Image 推到 MainWindow.video_frame.
/// 回调跑在 GStreamer 流线程 — 只拷数据 + invoke, 别碰 UI 状态
pub fn make_frame_sink(ui: slint::Weak<crate::MainWindow>) -> ipcam_gst::VideoFrameSink {
    std::sync::Arc::new(parking_lot::Mutex::new(
        move |f: ipcam_gst::RawVideoFrame| {
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(f.width, f.height);
            let dst = buf.make_mut_bytes();
            let row = f.width as usize * 4;
            for y in 0..f.height as usize {
                let src = &f.data[y * f.stride..y * f.stride + row];
                dst[y * row..(y + 1) * row].copy_from_slice(src);
            }
            // slint::Image 不是 Send (VRc 内部是 *mut), 不能跨线程移动;
            // SharedPixelBuffer 是纯像素存储可以 Send, 在事件循环里再包成 Image
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_video_frame(Image::from_rgba8(buf));
                }
            });
        },
    ))
}
