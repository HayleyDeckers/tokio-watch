use core::pin::Pin;
use core::task::{Context, Poll};
use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::borrow::BorrowMut;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::task::Waker;
use tokio::fs::File;
use tokio::io::AsyncRead;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

enum FileState {
    Modified,
    Deleted,
    //on linux, a file that gets deleted won't actually be deleted until all inode's to it have been removed.
    // NeedsReload,
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
    file: Option<tokio::fs::File>,
    last_seek_location: u64,
    shared_state: Arc<Mutex<SharedState>>,
    #[cfg(not(target_os = "windows"))]
    path: PathBuf,
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
        println!("watchedFile::poll");
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
                        println!("read without hittin EOF");
                        this.last_seek_location = 0;
                        return Poll::Ready(Ok(()));
                    }
                }
                Poll::Pending => {
                    println!("am pending...");
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            }
        }
        println!("EOF reached");
        //else if eof has been reached
        let (file, state, seek_loc) = {
            (
                this.file.take(),
                // core::pin::Pin::new_unchecked(&mut this.updated),
                &mut this.shared_state,
                &mut this.last_seek_location,
            )
        };
        //todo: extend lock to
        match state.lock().unwrap().borrow_mut().state {
            FileState::Modified => {
                println!("in modified state..");
                match file.map(|f| f.try_into_std()) {
                    Some(Ok(mut file)) => {
                        if *seek_loc >= 1 {
                            //we were at EOF, got awoken by a modify event, but still hit EOF
                            // this means that whatever modification happend can't have been an append
                            // and we have lost track of the state of the file, so we return to the start of the file.
                            // this is similar to how tail -f behaves.
                            //
                            // Note that most editors will also trigger this.
                            eprintln!("file truncated");
                            *seek_loc = file.seek(SeekFrom::Start(0)).unwrap();
                        } else {
                            *seek_loc = 1;
                        }
                        // on windows, events are emmitted if our file is deleted or renamed even if
                        // there's still an open Fd (i.e. ours), so we can reuse our existing link
                        #[cfg(target_os = "windows")]
                        {
                            this.file.replace(tokio::fs::File::from_std(file));
                        }
                        // on Linux and possibly other OS (that i haven't been able to check yet) a remove event is only
                        // emitted after the last Fd pointing to the original file (inode technically) is closed, so we close
                        // and reopen our handle here to detect file deletion
                        //
                        // we could also is https://doc.rust-lang.org/std/path/struct.Path.html#method.is_file but
                        // that could trigger a race-issue where a file is deleted and then reconstructed, leaving us
                        // reading from the old inode while the file was actually truncated?
                        #[cfg(not(target_os = "windows"))]
                        {
                            match std::fs::File::open(self.path) {
                                Ok(file) => {
                                    this.file.replace(tokio::fs::File::from_std(file));
                                }
                                Err(e) => {
                                    //the file couldn't be opened for whatever reason.
                                    // so we return EOF.
                                    //
                                    // todo: Technically the file could still exist, perhaps it was recreated with new permissions and we are now not allowed to open it
                                    // but for now this will do.
                                    return Poll::Ready(Ok());
                                }
                            };
                        }
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
            FileState::Deleted => {println!("in deleted state.."); return Poll::Ready(Ok(()))},
        }
        state.lock().unwrap().borrow_mut().waker = Some(cx.waker().clone());
        println!("pending...");
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
        let mut waker = WakerWrapper {
            shared_state: shared_state.clone(),
        };
        let mut watcher = notify::recommended_watcher(move |res| {
            eprintln!("\t got result: {:?}\n", res);
            match res {
            Ok(Event {
                kind: EventKind::Modify(ModifyKind::Name(_)),
                ..
            })
            | Ok(Event {
                kind: EventKind::Remove(_),
                ..
            }) => {
                println!("file was removed");
                waker.wake(FileState::Deleted);
                return;
            }
            Ok(Event {
                kind: EventKind::Modify(_),
                ..
            }) => {
                println!("file was modified");
                pauser.wake(FileState::Modified);
            }
            Err(e) => println!("watch error: {:?}", e),
            _ => {println!("dont know this event");}
        }})?;
        watcher.configure(Config::PreciseEvents(true))?;
        watcher.watch(path.as_ref(), RecursiveMode::NonRecursive)?;

        Ok(Self {
            file: Some(file),
            shared_state: shared_state.clone(),
            _watcher: watcher,
            #[cfg(not(target_os = "windows"))]
            path: path.as_ref().into(),
            last_seek_location: 0,
        })
    }
}
