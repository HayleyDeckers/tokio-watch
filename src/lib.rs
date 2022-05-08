use core::pin::Pin;
use core::task::{Context, Poll};
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::borrow::BorrowMut;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::task::Waker;
use tokio::fs::File;
use tokio::io::AsyncRead;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

enum FileState {
    Modified,
    Deleted,
}

use std::sync::{Arc, Mutex};
struct SharedState {
    /// Whether or not the future is paused
    state: FileState,
    /// The waker for the task that `TimerFuture` is running on.
    /// The thread can use this after setting `completed = true` to tell
    /// `TimerFuture`'s task to wake up, see that `completed = true`, and
    /// move forward.
    waker: Option<Waker>,
}

struct Pauser {
    shared_state: Arc<Mutex<SharedState>>,
}

impl Pauser {
    // pub fn pause(&mut self) {
    //     let mut shared_state = self.shared_state.lock().unwrap();
    //     shared_state.paused = true;
    // }
    fn wake(&mut self, state: FileState) -> bool {
        let mut shared_state = self.shared_state.lock().unwrap();
        shared_state.state = state;
        // shared_state.paused = false;
        if let Some(waker) = shared_state.waker.take() {
            waker.wake();
            true
        } else {
            false
        }
    }
}

/*
File watching

initial state: read file as normal. Track total bytes read and last update seen.
on eof:
    - check a 'last update seen' flag vs a 'last update submitted' flag to see if file might have changed by checking neq. if so, update last seen and retry
    - once eof _and_ flags are equal, set waker and return pending.
    - when woken up check append vs. recreate
*/
pub struct WatchedFile {
    file: Option<tokio::fs::File>,
    last_seek_location: u64,
    shared_state: Arc<Mutex<SharedState>>,
    //only here to tie the lifetimes together
    _watcher: RecommendedWatcher,
}

impl AsyncRead for WatchedFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        // println!("watchedFile::poll");
        let this = unsafe { self.get_unchecked_mut() };
        let size_before_poll = buf.filled().len();

        let file = { &mut this.file };
        //we need to have a file in the option
        if let Some(file) = file {
            let file = unsafe { core::pin::Pin::new_unchecked(file) };
            // let mut eof_reached = false;
            //which we then poll read
            match file.poll_read(cx, buf) {
                //and it returns as normal _if_ EOF hasn't been reached.
                Poll::Ready(Ok(())) => {
                    let eof_reached = size_before_poll == buf.filled().len();
                    if !eof_reached {
                        // println!("read without hittin EOF");
                        this.last_seek_location = 0;
                        return Poll::Ready(Ok(()));
                    }
                }
                Poll::Pending => {
                    // println!("am pending...");
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            }
        }
        // println!("EOF reached");
        //else if eof has been reached
        let (file, state, seek_loc) = {
            (
                this.file.take(),
                // core::pin::Pin::new_unchecked(&mut this.updated),
                &mut this.shared_state,
                &mut this.last_seek_location,
            )
        };
        match state.lock().unwrap().borrow_mut().state {
            FileState::Modified => {
                match file.map(|f| f.try_into_std()) {
                    Some(Ok(mut file)) => {
                        if *seek_loc >= 1 {
                            eprintln!("file truncated");
                            *seek_loc = file.seek(SeekFrom::Start(0)).unwrap();
                            // println!("seeked to {}", seek_loc);
                        } else {
                            // println!("incrementing seek_loc");
                            *seek_loc = 1;
                        }
                        // println!("hit EOF at {}", this.last_seek_location);
                        this.file.replace(tokio::fs::File::from_std(file));
                    }
                    Some(Err(file)) => {
                        this.file.replace(file);
                    }
                    None => {
                        unreachable!("file can't be none?");
                    }
                }
            }
            //file was deleted and we read all the contents still buffered.
            FileState::Deleted => return Poll::Ready(Ok(())),
        }

        state.lock().unwrap().borrow_mut().waker = Some(cx.waker().clone());
        return Poll::Pending;
    }
    // if eof_reached {}
}

impl WatchedFile {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let shared_state = Arc::new(Mutex::new(SharedState {
            state: FileState::Modified,
            waker: None,
        }));
        let file = File::open(path.as_ref()).await?;
        let mut pauser = Pauser {
            shared_state: shared_state.clone(),
        };
        let mut watcher = notify::recommended_watcher(move |res| match res {
            Ok(Event {
                kind: EventKind::Modify(ModifyKind::Name(_)),
                ..
            })
            | Ok(Event {
                kind: EventKind::Remove(_),
                ..
            }) => {
                println!("file was removed");
                pauser.wake(FileState::Deleted);
                return;
            }
            Ok(Event {
                kind: EventKind::Modify(_),
                ..
            }) => {
                // println!("file was modified");
                pauser.wake(FileState::Modified);
            }
            Err(e) => println!("watch error: {:?}", e),
            _ => {}
        })?;
        watcher.configure(Config::PreciseEvents(true))?;
        watcher.watch(path.as_ref(), RecursiveMode::NonRecursive)?;

        Ok(Self {
            file: Some(file),
            shared_state: shared_state.clone(),
            _watcher: watcher,
            last_seek_location: 0,
        })
    }
}
