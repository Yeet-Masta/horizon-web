//! Screenshot GPU waits happen on a worker; the owning thread receives mutable pixels.
use super::screenshot::{HdrScreenshot, PendingScreenshot};
use super::{Result, ScreenshotRequest, hdr10_to_rgb16, padded_row, square_downscale};
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(super) struct Reader {
    pub device: wgpu::Device,
    pub config: wgpu::SurfaceConfiguration,
}
impl Reader {
    /// The pixels `copy_frame` read back into `buffer`, 4 bytes each, row after row.
    fn read_frame(&self, buffer: &wgpu::Buffer) -> Result<Vec<[u8; 4]>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer.map_async(wgpu::MapMode::Read, .., move |r| {
            let _ = sender.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        receiver
            .recv()
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        self.mapped_frame(buffer)
    }
    /// The pixels of a mapped copy of the frame, which is unmapped after.
    fn mapped_frame(&self, buffer: &wgpu::Buffer) -> Result<Vec<[u8; 4]>> {
        let (width, height, row) = (
            self.config.width as usize,
            self.config.height as usize,
            padded_row(self.config.width) as usize,
        );
        let mut pixels = Vec::with_capacity(width * height);
        {
            let data = buffer.get_mapped_range(..).map_err(|e| e.to_string())?;
            for y in 0..height {
                pixels.extend_from_slice(data[y * row..y * row + width * 4].as_chunks::<4>().0);
            }
        }
        buffer.unmap();
        Ok(pixels)
    }
    /// The `size` icon `request` asks for, averaged straight from the mapped copy of the
    /// frame, which is unmapped after: the page's thread made it in the mapping's callback,
    /// and the frame copied twice over first took it 100 ms of a world's join.
    #[cfg(target_family = "wasm")]
    fn mapped_icon(
        &self,
        request: &ScreenshotRequest,
        size: [u32; 2],
        buffer: &wgpu::Buffer,
    ) -> std::result::Result<PendingScreenshot, String> {
        let (width, height) = (self.config.width, self.config.height);
        let row = padded_row(width) as usize;
        let bgra = matches!(
            self.config.format.remove_srgb_suffix(),
            wgpu::TextureFormat::Bgra8Unorm
        );
        let pixels = {
            let data = buffer.get_mapped_range(..).map_err(|e| e.to_string())?;
            super::square_downscale_by(
                |x, y| {
                    let p = y as usize * row + x as usize * 4;
                    let px = &data[p..p + 3];
                    if bgra {
                        [px[2], px[1], px[0]]
                    } else {
                        [px[0], px[1], px[2]]
                    }
                },
                width,
                height,
                size,
            )
        };
        buffer.unmap();
        Ok(PendingScreenshot {
            path: request.path.clone(),
            background: request.background,
            no_replace: false,
            image: mod_api::ScreenshotImage::new(size[0], size[1], pixels),
            hdr: None,
        })
    }
    pub(super) fn read_screenshot(
        &self,
        request: &ScreenshotRequest,
        buffer: &wgpu::Buffer,
        hdr_buffer: Option<&wgpu::Buffer>,
    ) -> Result<PendingScreenshot> {
        let frame = self.read_frame(buffer)?;
        let hdr = hdr_buffer.and_then(|buffer| match self.read_frame(buffer) {
            Ok(pixels) => Some(HdrScreenshot {
                size: [self.config.width, self.config.height],
                pixels: hdr10_to_rgb16(&pixels),
            }),
            Err(e) => {
                log::warn!("Horizon renderer: HDR screenshot readback: {e}");
                None
            }
        });
        Ok(self.screenshot(request, frame, hdr))
    }
    /// The screenshot `request` asked for from the frame's `frame` pixels.
    fn screenshot(
        &self,
        request: &ScreenshotRequest,
        frame: Vec<[u8; 4]>,
        hdr: Option<HdrScreenshot>,
    ) -> PendingScreenshot {
        let bgra = matches!(
            self.config.format.remove_srgb_suffix(),
            wgpu::TextureFormat::Bgra8Unorm
        );
        let mut pixels = Vec::with_capacity((self.config.width * self.config.height * 4) as usize);
        for px in frame {
            let (r, b) = if bgra { (px[2], px[0]) } else { (px[0], px[2]) };
            pixels.extend_from_slice(&[r, px[1], b, 255]);
        }
        // NativeImage.resizeSubRectTo on takeAutoScreenshot's grab: the icon
        // is the frame's centre square averaged down to its size.
        let (width, height, pixels) = match request.size {
            Some(size) => (
                size[0],
                size[1],
                square_downscale(&pixels, self.config.width, self.config.height, size),
            ),
            None => (self.config.width, self.config.height, pixels),
        };
        PendingScreenshot {
            path: request.path.clone(),
            background: request.background,
            no_replace: false,
            image: mod_api::ScreenshotImage::new(width, height, pixels),
            hdr,
        }
    }
}

type Outcome = std::result::Result<PendingScreenshot, String>;

#[derive(Default)]
pub(super) struct Readbacks(
    Vec<Receiver<Outcome>>,
    #[cfg(test)] Vec<horizon_std::thread::JoinHandle<()>>,
);
impl Readbacks {
    /// No readback is running or waiting to be polled.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn start(
        &mut self,
        reader: Reader,
        request: ScreenshotRequest,
        buffer: wgpu::Buffer,
        hdr: Option<wgpu::Buffer>,
    ) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        // A browser maps the copy once the page's event loop turns, and no thread may wait for
        // it there: the screenshot is made in the mapping's callback (the web build has no HDR).
        #[cfg(target_family = "wasm")]
        {
            let _ = hdr;
            let frame = buffer.clone();
            buffer.map_async(wgpu::MapMode::Read, .., move |r| {
                let result = r
                    .map_err(|e| e.to_string())
                    .and_then(|()| match request.size {
                        Some(size) => reader.mapped_icon(&request, size, &frame),
                        None => reader
                            .mapped_frame(&frame)
                            .map(|frame| reader.screenshot(&request, frame, None))
                            .map_err(|e| e.to_string()),
                    });
                let _ = sender.send(result);
            });
            self.0.push(receiver);
            return Ok(());
        }
        #[cfg(not(target_family = "wasm"))]
        let worker = horizon_std::thread::Builder::new()
            .name("Screenshot readback".into())
            .spawn(move || {
                let result = reader
                    .read_screenshot(&request, &buffer, hdr.as_ref())
                    .map_err(|e| e.to_string());
                let _ = sender.send(result);
            })?;
        #[cfg(all(test, not(target_family = "wasm")))]
        self.1.push(worker);
        #[cfg(all(not(test), not(target_family = "wasm")))]
        drop(worker);
        #[cfg(not(target_family = "wasm"))]
        self.0.push(receiver);
        #[cfg(not(target_family = "wasm"))]
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Outcome> {
        let outcome = match self.0.first()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("screenshot readback worker stopped".into()),
        };
        self.0.remove(0);
        Some(outcome)
    }
}

#[cfg(test)]
impl Drop for Readbacks {
    fn drop(&mut self) {
        self.0.clear();
        let mut panic = None;
        for worker in self.1.drain(..) {
            if let Err(error) = worker.join() {
                panic.get_or_insert(error);
            }
        }
        if !horizon_std::thread::panicking()
            && let Some(error) = panic
        {
            std::panic::resume_unwind(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_std::time::Duration;

    #[test]
    #[ignore = "daily"]
    fn a_background_save_writes_the_png_off_the_render_thread() {
        let _gpu = crate::gfx::test_gpu::lock();
        let instance = crate::gfx::test_gpu::instance();
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else {
            return;
        };
        let Ok((device, queue)) = pollster::block_on(adapter.request_device(&Default::default()))
        else {
            return;
        };
        let dir = horizon_std::env::temp_dir().join(format!("horizon-shot-{}", std::process::id()));
        horizon_std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        let [w, h] = [3u32, 2];
        // A frame of blue-first pixels, each row padded as `copy_frame` reads them back.
        let row = padded_row(w) as usize;
        let mut bytes = vec![0u8; row * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                bytes[y * row + x * 4..][..4].copy_from_slice(&[10 * x as u8, 20, 200, 0]);
            }
        }
        let source = wgpu::util::DeviceExt::create_buffer_init(
            &device,
            &wgpu::util::BufferInitDescriptor {
                label: None,
                contents: &bytes,
                usage: wgpu::BufferUsages::COPY_SRC,
            },
        );
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&source, 0, &buffer, 0, bytes.len() as u64);
        queue.submit([encoder.finish()]);
        let request = ScreenshotRequest {
            path: path.clone(),
            size: None,
            scaled: false,
            background: true,
        };
        let mut readbacks = Readbacks::default();
        readbacks
            .start(
                Reader {
                    device,
                    config: wgpu::SurfaceConfiguration {
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        width: w,
                        height: h,
                        present_mode: wgpu::PresentMode::Immediate,
                        desired_maximum_frame_latency: 2,
                        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                        view_formats: vec![],
                        color_space: wgpu::SurfaceColorSpace::default(),
                    },
                },
                request,
                buffer,
                None,
            )
            .unwrap();
        let deadline = horizon_std::time::Instant::now() + Duration::from_secs(10);
        let pending = loop {
            if let Some(result) = readbacks.poll() {
                break result.unwrap();
            }
            assert!(
                horizon_std::time::Instant::now() < deadline,
                "never read back"
            );
            horizon_std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            !path.exists(),
            "the owning-thread hook must run before any file is written"
        );
        let save = pending.save_background(None);
        let (saved_path, result) = loop {
            if let Some(outcome) = save.try_complete() {
                break outcome;
            }
            assert!(
                horizon_std::time::Instant::now() < deadline,
                "never written"
            );
            horizon_std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(saved_path, path);
        assert_eq!(result.map(|saved| saved.size), Ok([w, h]));
        let decoder = png::Decoder::new(std::io::BufReader::new(
            horizon_std::fs::File::open(&path).unwrap(),
        ));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut out).unwrap();
        // Red first, opaque: the last pixel of the first row was [20, 20, 200] blue-first.
        assert_eq!(&out[8..12], &[200, 20, 20, 255]);
        horizon_std::fs::remove_dir_all(&dir).unwrap();
    }
}
