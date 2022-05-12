use core::pin::Pin;
use core::task::{Context, Poll};
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::borrow::BorrowMut;
use std::future::Future;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::task::Waker;
use tokio::fs::File;
use tokio::io::AsyncRead;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy)]
enum FileState {
    Modified,
    Deleted,
    WaitingEOF,
    //on linux, a file that gets deleted won't actually be deleted until all inode's to it have been removed.
    // NeedsReload,
}

enum FileOpenState {
    Closed,
    Open(File),
    Opening(Pin<Box<dyn Future<Output = tokio::io::Result<File>> + Send>>),
}

use std::sync::{Arc, Mutex};
struct SharedState {
    //todo: probably want to make this into some kind of queue instead of just the last seen event.
    state: FileState,
    waker: Option<Waker>,
}

struct WakerWrapper {
    shared_state: Arc<Mutex<SharedState>>,
}

impl WakerWrapper {
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
    file: FileOpenState,
    last_seek_location: u64,
    at_eof: bool,
    shared_state: Arc<Mutex<SharedState>>,
    //on non-windows OS we need to close and reopen the file occasionaly to detect deletes.
    // so we remember the PathBuf to pass to tokio::file::Open
    // #[cfg(not(target_os = "windows"))]
    path: std::path::PathBuf,
    //only here to tie the lifetimes together
    _watcher: RecommendedWatcher,
}

impl AsyncRead for WatchedFile {
    //todo: cleanup pins and extend guarantess
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        let this = unsafe { self.get_unchecked_mut() };
        let size_before_poll = buf.filled().len();

        //if the file was close last poll, start a future to open it.
        if let FileOpenState::Closed = &mut this.file {
            //we box::pin the future because tokio doesn't return a concrete type here
            this.at_eof = false;
            this.file = FileOpenState::Opening(Box::pin(File::open(this.path.clone())));
        }
        if let FileOpenState::Opening(fut) = &mut this.file {
            //if the file was currently being opened, drive that future
            match Pin::new(fut).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => {
                    //todo: handle file not found => EOF
                    return Poll::Ready(Err(e));
                }
                //if the file is opened succesfully, set this.file and continue as normal.
                Poll::Ready(Ok(file)) => {
                    this.file = FileOpenState::Open(file);
                }
            }
        }

        //take the file out of the pin
        return if let FileOpenState::Open(file) = &mut this.file {
            //and pin it in place here.
            // this is safe because we have a reference to member of a pinned struct
            let file = unsafe { core::pin::Pin::new_unchecked(file) };
            //try and read from the file into the buffer
            match file.poll_read(cx, buf) {
                Poll::Pending => {
                    //return pending as-is
                    Poll::Pending
                }
                //return errors as-is
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    let bytes_read = buf.filled().len() - size_before_poll;
                    this.last_seek_location += bytes_read as u64;
                    if bytes_read != 0 {
                        //as long as the file has not reached EOF we return the results as normal
                        this.at_eof = false;
                        Poll::Ready(Ok(()))
                    } else {
                        let mut lock = this.shared_state.lock().unwrap();
                        let shared_state = lock.borrow_mut();
                        match shared_state.state {
                            FileState::Deleted => {
                                //we hit EOF on our open file descriptor and the OS has reported that the file has been deleted sometime between us opening and hitting EOF
                                // so this is truely EOF.
                                Poll::Ready(Ok(()))
                            }
                            _ => {
                                //we've hit EOF but the file hasn't been deleted yet
                                //so we tell the file watched how to wake us
                                shared_state.waker = Some(cx.waker().clone());
                                //and set the current state to 'waiting for new events.
                                shared_state.state = FileState::WaitingEOF;
                                //if we hit EOF twice in a row, that means the file was most likely truncated.
                                // reseek to zero.
                                if this.at_eof {
                                    //...
                                    // either reseek to zero (maybe reopen file?)
                                    // or return an error?
                                    eprintln!("File truncated");
                                    this.file = FileOpenState::Closed;
                                    cx.waker().wake_by_ref();
                                    Poll::Pending
                                } else {
                                    //mark that last poll we were at EOF.
                                    this.at_eof = true;
                                    //on Linux (and possibly other non-windows OS) we have to close our file to trigger a delete event from being generated.
                                    #[cfg(not(target_os = "windows"))]
                                    {
                                        // this.file = FileOpenState::Closed
                                    }

                                    Poll::Pending
                                }
                            }
                        }
                    }
                }
            }
        } else {
            unreachable!("we already exhausted all other options")
        };
    }
}

impl WatchedFile {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let shared_state = Arc::new(Mutex::new(SharedState {
            state: FileState::Modified,
            waker: None,
        }));
        let file = File::open(path.as_ref()).await?;
        let mut waker = WakerWrapper {
            shared_state: shared_state.clone(),
        };
        let mut watcher = notify::recommended_watcher(move |res| {
            // eprintln!("\t got result: {:?}\n", res);
            match res {
                Ok(Event {
                    kind: EventKind::Modify(ModifyKind::Name(_)),
                    ..
                })
                | Ok(Event {
                    kind: EventKind::Remove(_),
                    ..
                }) => {
                    // println!("file was removed");
                    waker.wake(FileState::Deleted);
                    return;
                }
                Ok(Event {
                    kind: EventKind::Modify(_),
                    ..
                }) => {
                    // println!("file was modified");
                    waker.wake(FileState::Modified);
                }
                Err(e) => println!("watch error: {:?}", e),
                _ => { /*println!("dont know this event");*/ }
            }
        })?;
        watcher.configure(Config::PreciseEvents(true))?;
        watcher.watch(path.as_ref(), RecursiveMode::NonRecursive)?;

        Ok(Self {
            file: FileOpenState::Open(file),
            shared_state: shared_state.clone(),
            at_eof: false,
            _watcher: watcher,
            // #[cfg(not(target_os = "windows"))]
            path: path.as_ref().into(),
            last_seek_location: 0,
        })
    }
}
