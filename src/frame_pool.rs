use crossbeam_channel::{Receiver, RecvError, SendError, Sender, bounded};

#[derive(Debug, Clone)]
pub struct FramePool {
    free_rx: Receiver<Vec<u8>>,
    free_tx: Sender<Vec<u8>>,
}

impl FramePool {
    /// `num_buffers` is the number of different buffers allocated. The more you have, the more
    /// memory you allocate.
    ///
    /// `frame_size` is the number of bytes each buffer contains. If your pixels are in the form of
    /// RGBA8 (it is for us), then this should be equal to `width * height * 4` since each pixel is
    /// 4 bytes long.
    pub fn new(num_buffers: usize, frame_size: usize) -> Self {
        let (tx, rx) = bounded(num_buffers);
        for _ in 0..num_buffers {
            tx.send(vec![0u8; frame_size])
                .expect("Couldn't setup buffers for ffmpeg");
        }
        Self {
            free_tx: tx,
            free_rx: rx,
        }
    }

    pub fn get(&self) -> Result<Vec<u8>, RecvError> {
        return self.free_rx.recv();
    }

    pub fn recycle(&self, buf: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        return self.free_tx.send(buf);
    }
}
