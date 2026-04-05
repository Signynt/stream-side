use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use pipewire::spa::pod::PropertyFlags;
use pipewire as pw;
use pipewire::context::ContextBox;
use pipewire::main_loop::MainLoopBox;
use pw::properties::properties;
use std::os::fd::FromRawFd;
use std::os::unix::io::IntoRawFd;
use sender::encode::{Encoder, process_frame_from_pw_buffer};
use std::net::SocketAddr;
use std::env;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    // Сендер теперь — QUIC-сервер: слушаем входящие подключения.
    // По умолчанию — 0.0.0.0:4433 (принимаем со всех интерфейсов).
    let listen_addr: SocketAddr = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("0.0.0.0:4433")
        .parse()
        .unwrap_or_else(|_| {
            eprintln!("❌ Неверный формат адреса. Используем 0.0.0.0:4433");
            "0.0.0.0:4433".parse().unwrap()
        });

    println!("🚀 Sender стартует как QUIC-сервер на {}", listen_addr);
    println!("   Клиенты (receiver) должны подключаться к <your-ip>:{}", listen_addr.port());

    let (node_id, raw_fd) = run_portal().await.expect("Portal failed");

    std::thread::spawn(move || {
        run_pipewire(node_id, raw_fd, listen_addr);
    }).join().unwrap();
}

async fn run_portal() -> ashpd::Result<(u32, i32)> {
    let proxy   = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;

    proxy.select_sources(&session, SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot)).await?;

    let response = proxy.start(&session, None, Default::default()).await?.response()?;
    let node_id  = response.streams()[0].pipe_wire_node_id();
    let fd       = proxy.open_pipe_wire_remote(&session, Default::default()).await?;

    Ok((node_id, fd.into_raw_fd()))
}

fn run_pipewire(node_id: u32, raw_fd: i32, listen_addr: SocketAddr) {
    pw::init();
    let mainloop = MainLoopBox::new(None).unwrap();
    let context  = ContextBox::new(&mainloop.loop_(), None).unwrap();
    let core     = context.connect_fd(
        unsafe { std::os::unix::io::OwnedFd::from_raw_fd(raw_fd) }, None,
    ).unwrap();

    let stream = pw::stream::StreamBox::new(&core, "capture", properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
        "pipewire.display.rate"   => "144/1",
    }).unwrap();

    let mut encoder: Option<Encoder> = None;

    let mut last_check = std::time::Instant::now();
    let mut frames     = 0u32;

    let _listener = stream.add_local_listener::<()>()
        .process(move |stream, _| {
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() { return; }

            unsafe {
                process_frame_from_pw_buffer(raw, |src| {
                    let enc = encoder.get_or_insert_with(|| {
                        Encoder::new(1920, 1080, listen_addr)
                    });
                    let _ = enc.encode(src);
                    frames += 1;
                });

                if last_check.elapsed().as_secs() >= 1 {
                    println!("FPS: {}", frames);
                    frames     = 0;
                    last_check = std::time::Instant::now();
                }

                stream.queue_raw_buffer(raw);
            }
        })
        .register().unwrap();

    let binding = spa_video_params();
    let mut params = [pw::spa::pod::Pod::from_bytes(&binding).unwrap()];
    stream.connect(
        pw::spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ).unwrap();

    println!("📺 Захват запущен. Ожидаем подключения клиентов. Ctrl+C для остановки.");
    mainloop.run();
}

fn spa_video_params() -> Vec<u8> {
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::sys::*;

    let value = pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: SPA_TYPE_OBJECT_Format,
        id:    SPA_PARAM_EnumFormat,
        properties: vec![
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaType,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_TYPE_video)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_mediaSubtype,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_MEDIA_SUBTYPE_raw)),
            },
            pw::spa::pod::Property {
                key:   SPA_FORMAT_VIDEO_format,
                flags: PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_FORMAT_BGRA)),
            },
        ],
    });

    let mut bytes = Vec::new();
    PodSerializer::serialize(&mut std::io::Cursor::new(&mut bytes), &value).unwrap();
    bytes
}
