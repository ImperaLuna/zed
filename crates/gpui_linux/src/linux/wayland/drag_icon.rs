use std::{
    fs::File,
    io::Write as _,
    os::fd::{AsFd as _, FromRawFd as _, OwnedFd},
};

use anyhow::Context as _;
use gpui::DragPicture;
use wayland_client::{
    Proxy as _, QueueHandle,
    protocol::{wl_buffer, wl_shm, wl_surface},
};

use crate::linux::wayland::client::{Globals, WaylandClientStatePtr};

const BYTES_PER_PIXEL: usize = 4;

/// The surface handed to `wl_data_device.start_drag` as the drag icon.
pub(crate) struct DragIcon {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
}

impl DragIcon {
    pub(crate) fn new(globals: &Globals, picture: &DragPicture) -> anyhow::Result<Self> {
        let pixels = Pixels::of(picture)?;
        let buffer = shm_buffer(&globals.shm, &globals.qh, &pixels)?;
        let surface = globals.compositor.create_surface(&globals.qh, ());
        surface.set_buffer_scale(pixels.scale);
        Ok(Self { surface, buffer })
    }

    pub(crate) fn surface(&self) -> &wl_surface::WlSurface {
        &self.surface
    }

    /// Shows the picture. The icon surface only gets its role from
    /// `start_drag`, so this runs after it.
    pub(crate) fn show(&self, picture: &DragPicture) {
        let x = logical(picture.offset.x);
        let y = logical(picture.offset.y);
        if self.surface.version() >= wl_surface::REQ_OFFSET_SINCE {
            self.surface.offset(x, y);
            self.surface.attach(Some(&self.buffer), 0, 0);
        } else {
            self.surface.attach(Some(&self.buffer), x, y);
        }
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }

    pub(crate) fn destroy(self) {
        self.surface.destroy();
        self.buffer.destroy();
    }
}

fn logical(value: gpui::Pixels) -> i32 {
    f32::from(value.round()) as i32
}

struct Pixels {
    width: i32,
    height: i32,
    scale: i32,
    bytes: Vec<u8>,
}

impl Pixels {
    /// `wl_shm` ARGB8888 is premultiplied and little-endian (B, G, R, A in
    /// memory), and a scaled buffer's sides must divide by its scale, so the
    /// image is premultiplied and padded with transparent pixels.
    fn of(picture: &DragPicture) -> anyhow::Result<Self> {
        let size = picture.image.size(0);
        let source = picture
            .image
            .as_bytes(0)
            .context("drag picture has no frame")?;
        let source_width = usize::try_from(size.width.0)?;
        let source_height = usize::try_from(size.height.0)?;
        anyhow::ensure!(
            source_width > 0 && source_height > 0,
            "drag picture is empty"
        );
        let scale = picture.scale.max(1);
        let step = usize::try_from(scale)?;
        let width = source_width.div_ceil(step) * step;
        let height = source_height.div_ceil(step) * step;
        let mut bytes = vec![0; width * height * BYTES_PER_PIXEL];
        for (row, source_row) in source
            .chunks_exact(source_width * BYTES_PER_PIXEL)
            .take(source_height)
            .enumerate()
        {
            let start = row * width * BYTES_PER_PIXEL;
            let target = &mut bytes[start..start + source_width * BYTES_PER_PIXEL];
            for (pixel, source_pixel) in target
                .chunks_exact_mut(BYTES_PER_PIXEL)
                .zip(source_row.chunks_exact(BYTES_PER_PIXEL))
            {
                let alpha = u16::from(source_pixel[3]);
                for channel in 0..3 {
                    pixel[channel] = premultiply(source_pixel[channel], alpha);
                }
                pixel[3] = source_pixel[3];
            }
        }
        Ok(Self {
            width: i32::try_from(width)?,
            height: i32::try_from(height)?,
            scale: i32::try_from(scale)?,
            bytes,
        })
    }
}

fn premultiply(channel: u8, alpha: u16) -> u8 {
    ((u16::from(channel) * alpha + 127) / 255) as u8
}

fn shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<WaylandClientStatePtr>,
    pixels: &Pixels,
) -> anyhow::Result<wl_buffer::WlBuffer> {
    let file = memfd()?;
    (&file).write_all(&pixels.bytes)?;
    let length = i32::try_from(pixels.bytes.len())?;
    let pool = shm.create_pool(file.as_fd(), length, qh, ());
    let stride = pixels.width * i32::try_from(BYTES_PER_PIXEL)?;
    let buffer = pool.create_buffer(
        0,
        pixels.width,
        pixels.height,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

fn memfd() -> anyhow::Result<File> {
    // SAFETY: the name is a NUL-terminated literal and the flags are valid.
    let raw = unsafe { libc::memfd_create(c"gpui-drag-icon".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create for the drag icon");
    }
    // SAFETY: memfd_create returned a new descriptor that nothing else owns.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(File::from(owned))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{RenderImage, point, px};
    use image::{Frame, RgbaImage};

    use super::*;

    fn picture(width: u32, height: u32, bgra: [u8; 4], scale: u32) -> DragPicture {
        let image = RgbaImage::from_fn(width, height, |_, _| image::Rgba(bgra));
        DragPicture {
            image: Arc::new(RenderImage::new([Frame::new(image)])),
            scale,
            offset: point(px(4.), px(6.)),
        }
    }

    #[test]
    fn pixels_are_premultiplied() {
        let pixels = Pixels::of(&picture(1, 1, [200, 100, 50, 128], 1)).unwrap();
        assert_eq!(pixels.bytes, [100, 50, 25, 128]);
    }

    #[test]
    fn scaled_pictures_are_padded_to_whole_logical_pixels() {
        let pixels = Pixels::of(&picture(3, 5, [255, 255, 255, 255], 2)).unwrap();
        assert_eq!((pixels.width, pixels.height, pixels.scale), (4, 6, 2));
        assert_eq!(pixels.bytes.len(), 4 * 6 * BYTES_PER_PIXEL);
        assert_eq!(&pixels.bytes[..4], [255, 255, 255, 255]);
        assert_eq!(&pixels.bytes[3 * 4..4 * 4], [0, 0, 0, 0]);
        assert_eq!(&pixels.bytes[5 * 4 * 4..], [0; 4 * 4]);
    }

    #[test]
    fn an_empty_picture_is_refused() {
        assert!(Pixels::of(&picture(0, 0, [0, 0, 0, 0], 1)).is_err());
    }
}
