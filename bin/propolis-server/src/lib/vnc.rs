use async_trait::async_trait;
use propolis::common::GuestAddr;
use propolis::dispatch::AsyncCtx;
use propolis::hw::qemu::ramfb::{Config, FramebufferSpec};
use rfb::encodings::RawEncoding;
use rfb::pixel_formats::fourcc;
use rfb::rfb::{
    FramebufferUpdate, ProtoVersion, Rectangle, SecurityType, SecurityTypes,
};
use rfb::server::{Server, VncServer, VncServerConfig, VncServerData};
use slog::{debug, error, info, o, Logger};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy)]
pub struct RamFb {
    addr: u64,
    width: u32,
    height: u32,
    fourcc: u32,
}

impl RamFb {
    pub fn new(fb_spec: FramebufferSpec) -> Self {
        Self {
            addr: fb_spec.addr,
            width: fb_spec.width,
            height: fb_spec.height,
            fourcc: fb_spec.fourcc,
        }
    }
}

struct DefaultFb {
    width: u16,
    height: u16,
}

enum Framebuffer {
    Uninitialized(DefaultFb),
    Initialized(RamFb),
}

struct PropolisVncServerInner {
    framebuffer: Framebuffer,
    actx: Option<AsyncCtx>,
    vnc_server: Option<VncServer<PropolisVncServer>>,
}

#[derive(Clone)]
pub struct PropolisVncServer {
    inner: Arc<Mutex<PropolisVncServerInner>>,
    log: Logger,
}

impl PropolisVncServer {
    pub fn new(initial_width: u16, initial_height: u16, log: Logger) -> Self {
        PropolisVncServer {
            inner: Arc::new(Mutex::new(PropolisVncServerInner {
                framebuffer: Framebuffer::Uninitialized(DefaultFb {
                    width: initial_width,
                    height: initial_height,
                }),
                actx: None,
                vnc_server: None,
            })),
            log,
        }
    }

    pub async fn initialize(
        &self,
        fb: RamFb,
        actx: AsyncCtx,
        vnc_server: VncServer<Self>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.framebuffer = Framebuffer::Initialized(fb);
        inner.actx = Some(actx);
        inner.vnc_server = Some(vnc_server);
    }

    pub async fn update(&self, config: &Config, is_valid: bool) {
        if is_valid {
            debug!(self.log, "updating framebuffer");

            let fb_spec = config.get_framebuffer_spec();
            let fb = RamFb::new(fb_spec);

            let mut inner = self.inner.lock().await;

            if fb.addr != 0 {
                inner.framebuffer = Framebuffer::Initialized(fb);
            }

            match fourcc::fourcc_to_pixel_format(fb.fourcc) {
                Ok(pf) => {
                    inner
                        .vnc_server
                        .as_ref()
                        .unwrap()
                        .set_pixel_format(pf)
                        .await;
                    info!(
                        self.log,
                        "pixel format set to fourcc={:#04x}", fb.fourcc
                    );
                }
                Err(e) => {
                    error!(self.log, "could not set pixel format: {:?}", e);
                }
            }
        } else {
            error!(self.log, "invalid config update");
        }
    }
}

#[async_trait]
impl Server for PropolisVncServer {
    async fn get_framebuffer_update(&self) -> FramebufferUpdate {
        let inner = self.inner.lock().await;

        let fb = match &inner.framebuffer {
            Framebuffer::Uninitialized(fb) => {
                debug!(self.log, "framebuffer: uninitialized");

                // Display a white screen if the guest isn't ready yet.
                let len: usize = fb.width as usize * fb.height as usize * 4;
                let pixels = vec![0xffu8; len];

                let r = Rectangle::new(
                    0,
                    0,
                    fb.width,
                    fb.height,
                    Box::new(RawEncoding::new(pixels)),
                );
                FramebufferUpdate::new(vec![r])
            }
            Framebuffer::Initialized(fb) => {
                debug!(self.log, "framebuffer initialized: fb={:?}", fb);

                let len = fb.height as usize * fb.width as usize * 4;
                let mut buf = vec![0u8; len];

                let memctx = inner
                    .actx
                    .as_ref()
                    .unwrap()
                    .dispctx()
                    .await
                    .unwrap()
                    .mctx
                    .memctx();
                let read = memctx.read_into(GuestAddr(fb.addr), &mut buf, len);
                drop(memctx);

                assert!(read.is_some());
                debug!(self.log, "read {} bytes from guest", read.unwrap());

                let r = Rectangle::new(
                    0,
                    0,
                    fb.width as u16,
                    fb.height as u16,
                    Box::new(RawEncoding::new(buf)),
                );
                FramebufferUpdate::new(vec![r])
            }
        };

        fb
    }
}

// Default VNC server configuration.
pub fn setup_vnc(
    log: &Logger,
    addr: SocketAddr,
) -> VncServer<PropolisVncServer> {
    let initial_width = 1024;
    let initial_height = 768;

    let config = VncServerConfig {
        addr,
        version: ProtoVersion::Rfb38,
        // vncviewer won't work without offering VncAuth, even though it doesn't ask to use
        // it.
        sec_types: SecurityTypes(vec![
            SecurityType::None,
            SecurityType::VncAuthentication,
        ]),
        name: "propolis-vnc".to_string(),
    };

    let pf = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_XR24).unwrap();
    let data = VncServerData {
        width: initial_width,
        height: initial_height,
        input_pixel_format: pf,
    };
    let pvnc = PropolisVncServer::new(
        initial_width,
        initial_height,
        log.new(o!("component" => "vnc-server")),
    );

    VncServer::new(pvnc, config, data)
}
