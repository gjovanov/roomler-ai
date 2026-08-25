use quartz;
use std::{io, ops, mem};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, TryLockError};

pub struct Capturer {
    inner: quartz::Capturer,
    frame: Arc<Mutex<Option<quartz::Frame>>>
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let frame = Arc::new(Mutex::new(None));

        let f = frame.clone();
        let inner = quartz::Capturer::new(
            display.0,
            // ROOMLER PATCH: size the stream in PIXELS, not POINTS.
            //
            // Upstream passes `display.width()/height()`, which are
            // `CGDisplayPixelsWide/High` — misleadingly named, they return
            // POINTS. On a Retina panel that is half the real count each way,
            // so CoreGraphics downsampled the frame 4:1 with its own scaler
            // before the capturer ever saw it: text drawn at 2x arrived
            // already destroyed, and nothing downstream could recover it.
            //
            // `Display::width()/height()` deliberately keep returning POINTS —
            // pointer injection maps normalised coordinates into that space.
            display.0.pixel_width(),
            display.0.pixel_height(),
            quartz::PixelFormat::Argb8888,
            // ROOMLER PATCH: composite the cursor INTO the frame.
            //
            // Upstream passes `Default::default()`, whose `cursor` is FALSE.
            // On macOS that is not a cosmetic default — it is the only way a
            // viewer ever sees the remote pointer, because the cursor tracker
            // that streams a shape + position separately (`capture/cursor.rs`)
            // is Windows-only and returns `None` here. With `ShowCursor` off
            // and no tracker, a Mac session has no pointer at all: you cannot
            // see where you are about to click.
            //
            // Field-reported 2026-08-25 — the pointer was missing both for a
            // viewer AND when moving the Mac's own mouse, i.e. it was absent
            // from the captured frames themselves.
            //
            // ⚠️ Ask for it EXPLICITLY rather than relying on whatever
            // WindowServer happens to composite. Whether the hardware cursor
            // plane lands in a capture is not a documented guarantee, and it
            // can change with the stream's output size — which is exactly the
            // knob the patch above turns.
            quartz::Config {
                cursor: true,
                ..Default::default()
            },
            move |inner| {
                if let Ok(mut f) = f.lock() {
                    *f = Some(inner);
                }
            }
        ).map_err(|_| io::Error::from(io::ErrorKind::Other))?;

        Ok(Capturer { inner, frame })
    }

    pub fn width(&self) -> usize {
        self.inner.width()
    }

    pub fn height(&self) -> usize {
        self.inner.height()
    }

    pub fn frame<'a>(&'a mut self) -> io::Result<Frame<'a>> {
        match self.frame.try_lock() {
            Ok(mut handle) => {
                let mut frame = None;
                mem::swap(&mut frame, &mut handle);

                match frame {
                    Some(frame) =>
                        Ok(Frame(frame, PhantomData)),

                    None =>
                        Err(io::ErrorKind::WouldBlock.into())
                }
            }

            Err(TryLockError::WouldBlock) =>
                Err(io::ErrorKind::WouldBlock.into()),

            Err(TryLockError::Poisoned(..)) =>
                Err(io::ErrorKind::Other.into())
        }
    }
}

pub struct Frame<'a>(
    quartz::Frame,
    PhantomData<&'a [u8]>
);

impl<'a> Frame<'a> {
    /// ROOMLER PATCH: the surface's true row pitch, in bytes.
    ///
    /// `Deref`'s slice is `IOSurfaceGetAllocSize` bytes long (page-rounded),
    /// so `len() / height` is NOT the stride. Callers that walk rows must use
    /// this instead.
    pub fn bytes_per_row(&self) -> usize {
        self.0.bytes_per_row()
    }
}

impl<'a> ops::Deref for Frame<'a> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &*self.0
    }
}

pub struct Display(quartz::Display);

impl Display {
    pub fn primary() -> io::Result<Display> {
        Ok(Display(quartz::Display::primary()))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(
            quartz::Display::online()
                .map_err(|_| io::Error::from(io::ErrorKind::Other))?
                .into_iter()
                .map(Display)
                .collect()
        )
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }
}
