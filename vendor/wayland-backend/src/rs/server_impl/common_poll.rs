use std::{
    collections::{HashSet, VecDeque},
    os::unix::io::{AsRawFd, BorrowedFd, OwnedFd},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    handle::State, ClientId, Data, DispatchOutcome, GlobalHandler, GlobalId, Handle, InnerClientId,
    InnerGlobalId, InnerHandle, InnerObjectId, ObjectId,
};
use crate::{
    core_interfaces::{WL_DISPLAY_INTERFACE, WL_REGISTRY_INTERFACE},
    protocol::{same_interface, Argument, Message},
    rs::map::Object,
    types::server::InitError,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::event::{epoll, Timespec};

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos"
))]
use rustix::event::kqueue::*;
use smallvec::SmallVec;

#[derive(Debug)]
pub struct InnerBackend<D: 'static> {
    state: Arc<Mutex<State<D>>>,
    pending_clients: VecDeque<InnerClientId>,
    queued_clients: HashSet<InnerClientId>,
}

struct DispatchBudget {
    dispatched: usize,
    max_requests: usize,
    time_limit: Option<(Instant, Duration)>,
}

impl DispatchBudget {
    fn exhausted(&self) -> bool {
        self.dispatched >= self.max_requests
            || self.time_limit.map(|(start, duration)| start.elapsed() >= duration).unwrap_or(false)
    }
}

impl<D> InnerBackend<D> {
    pub fn new() -> Result<Self, InitError> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let poll_fd = epoll::create(epoll::CreateFlags::CLOEXEC)
            .map_err(Into::into)
            .map_err(InitError::Io)?;

        #[cfg(any(
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "macos"
        ))]
        let poll_fd = kqueue().map_err(Into::into).map_err(InitError::Io)?;

        Ok(Self {
            state: Arc::new(Mutex::new(State::new(poll_fd))),
            pending_clients: VecDeque::new(),
            queued_clients: HashSet::new(),
        })
    }

    pub fn flush(&self, client: Option<ClientId>) -> std::io::Result<()> {
        self.state.lock().unwrap().flush(client)
    }

    pub fn handle(&self) -> Handle {
        Handle { handle: InnerHandle { state: self.state.clone() as Arc<_> } }
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        let raw_fd = self.state.lock().unwrap().poll_fd.as_raw_fd();
        // This allows the lifetime of the BorrowedFd to be tied to &self rather than the lock guard,
        // which is the real safety concern
        unsafe { BorrowedFd::borrow_raw(raw_fd) }
    }

    pub fn dispatch_client(
        &mut self,
        data: &mut D,
        client_id: InnerClientId,
    ) -> std::io::Result<usize> {
        self.pending_clients.retain(|id| id != &client_id);
        self.queued_clients.remove(&client_id);
        let ret = self.dispatch_events_for(data, client_id);
        let cleanup = self.state.lock().unwrap().cleanup();
        cleanup(&self.handle(), data);
        ret
    }

    fn queue_client(&mut self, id: InnerClientId) {
        if self.queued_clients.insert(id.clone()) {
            self.pending_clients.push_back(id);
        }
    }

    // Sample one bounded batch, never drain readiness until empty: a producer
    // may remain readable indefinitely. Unfinished clients retain their position.
    // An interrupted sample must not hide retained work behind an error result.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn queue_ready_clients(&mut self) -> std::io::Result<()> {
        let mut events = Vec::<epoll::Event>::with_capacity(32);
        let ready = epoll::wait(
            self.poll_fd(),
            rustix::buffer::spare_capacity(&mut events),
            Some(&Timespec::default()),
        );
        if let Err(error) = ready {
            let error = std::io::Error::from(error);
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error);
        }
        for event in events {
            self.queue_client(InnerClientId::from_u64(event.data.u64()));
        }
        Ok(())
    }

    #[cfg(any(
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos"
    ))]
    fn queue_ready_clients(&mut self) -> std::io::Result<()> {
        let mut events = Vec::<Event>::with_capacity(32);
        let ready = unsafe {
            kevent(
                &self.poll_fd(),
                &[],
                rustix::buffer::spare_capacity(&mut events),
                Some(Duration::ZERO),
            )
        };
        if let Err(error) = ready {
            let error = std::io::Error::from(error);
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error);
        }
        for event in events {
            self.queue_client(InnerClientId::from_u64(event.udata() as u64));
        }
        Ok(())
    }

    pub fn dispatch_all_clients_with_budget(
        &mut self,
        data: &mut D,
        max_requests: usize,
        max_duration: Duration,
        client_quantum: usize,
    ) -> std::io::Result<DispatchOutcome> {
        let mut budget = DispatchBudget {
            dispatched: 0,
            max_requests,
            time_limit: Some((Instant::now(), max_duration)),
        };
        self.queue_ready_clients()?;
        while client_quantum != 0 && !budget.exhausted() {
            let id = match self.pending_clients.pop_front() {
                Some(id) => id,
                None => break,
            };
            self.queued_clients.remove(&id);
            let pending =
                self.dispatch_events_for_with_budget(data, id.clone(), client_quantum, &mut budget);
            // A yielded client may have no readable FD: next_request() can have
            // moved its entire socket payload into BufferedSocket already.
            if matches!(pending, Ok(true)) {
                self.queue_client(id);
            }
            let cleanup = self.state.lock().unwrap().cleanup();
            cleanup(&self.handle(), data);
            self.queue_ready_clients()?;
        }
        Ok(DispatchOutcome {
            dispatched: budget.dispatched,
            more_pending: !self.pending_clients.is_empty(),
        })
    }

    fn dispatch_pending_clients(&mut self, data: &mut D) -> usize {
        let mut dispatched = 0;
        while let Some(id) = self.pending_clients.pop_front() {
            self.queued_clients.remove(&id);
            if let Ok(count) = self.dispatch_events_for(data, id) {
                dispatched += count;
            }
            let cleanup = self.state.lock().unwrap().cleanup();
            cleanup(&self.handle(), data);
        }
        dispatched
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn dispatch_all_clients(&mut self, data: &mut D) -> std::io::Result<usize> {
        use std::os::unix::io::AsFd;

        let mut dispatched = self.dispatch_pending_clients(data);
        let poll_fd = self.poll_fd();
        let mut events = Vec::<epoll::Event>::with_capacity(32);
        loop {
            let buffer = rustix::buffer::spare_capacity(&mut events);
            epoll::wait(poll_fd.as_fd(), buffer, Some(&Timespec::default()))?;

            if events.is_empty() {
                break;
            }

            for event in events.drain(..) {
                let id = InnerClientId::from_u64(event.data.u64());
                // remove the cb while we call it, to gracefully handle reentrancy
                if let Ok(count) = self.dispatch_events_for(data, id) {
                    dispatched += count;
                }
            }
            let cleanup = self.state.lock().unwrap().cleanup();
            cleanup(&self.handle(), data);
        }

        Ok(dispatched)
    }

    #[cfg(any(
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos"
    ))]
    pub fn dispatch_all_clients(&mut self, data: &mut D) -> std::io::Result<usize> {
        use std::time::Duration;

        let mut dispatched = self.dispatch_pending_clients(data);
        let poll_fd = self.poll_fd();
        let mut events = Vec::<Event>::with_capacity(32);
        loop {
            let buffer = rustix::buffer::spare_capacity(&mut events);
            let nevents = unsafe { kevent(&poll_fd, &[], buffer, Some(Duration::ZERO))? };

            if nevents == 0 {
                break;
            }

            for event in events.drain(..) {
                let id = InnerClientId::from_u64(event.udata() as u64);
                // remove the cb while we call it, to gracefully handle reentrancy
                if let Ok(count) = self.dispatch_events_for(data, id) {
                    dispatched += count;
                }
            }
            let cleanup = self.state.lock().unwrap().cleanup();
            cleanup(&self.handle(), data);
        }

        Ok(dispatched)
    }

    pub(crate) fn dispatch_events_for(
        &self,
        data: &mut D,
        client_id: InnerClientId,
    ) -> std::io::Result<usize> {
        let mut budget =
            DispatchBudget { dispatched: 0, max_requests: usize::MAX, time_limit: None };
        self.dispatch_events_for_with_budget(data, client_id, usize::MAX, &mut budget)?;
        Ok(budget.dispatched)
    }

    fn dispatch_events_for_with_budget(
        &self,
        data: &mut D,
        client_id: InnerClientId,
        client_quantum: usize,
        budget: &mut DispatchBudget,
    ) -> std::io::Result<bool> {
        let initial_count = budget.dispatched;
        let handle = self.handle();
        let mut state = self.state.lock().unwrap();
        loop {
            // This must precede next_request(), not just the user callback:
            // display/registry requests and rejected requests can continue below.
            if budget.exhausted() || budget.dispatched - initial_count >= client_quantum {
                return Ok(true);
            }
            let action = {
                let state = &mut *state;
                if let Ok(client) = state.clients.get_client_mut(client_id.clone()) {
                    let (message, object) = match client.next_request() {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if budget.dispatched > initial_count {
                                break;
                            } else {
                                return Err(e);
                            }
                        }
                        Err(e) => {
                            #[cfg(any(target_os = "linux", target_os = "android"))]
                            {
                                epoll::delete(&state.poll_fd, client)?;
                            }

                            #[cfg(any(
                                target_os = "dragonfly",
                                target_os = "freebsd",
                                target_os = "netbsd",
                                target_os = "openbsd",
                                target_os = "macos"
                            ))]
                            {
                                use rustix::event::kqueue::*;
                                use std::os::unix::io::{AsFd, AsRawFd};

                                let evt = Event::new(
                                    EventFilter::Read(client.as_fd().as_raw_fd()),
                                    EventFlags::DELETE,
                                    client_id.as_u64() as *mut _,
                                );

                                let events: &mut [Event] = &mut [];
                                unsafe {
                                    kevent(&state.poll_fd, &[evt], events, None).map(|_| ())?;
                                }
                            }
                            return Err(e);
                        }
                    };
                    budget.dispatched += 1;
                    if same_interface(object.interface, &WL_DISPLAY_INTERFACE) {
                        client.handle_display_request(message, &mut state.registry);
                        continue;
                    } else if same_interface(object.interface, &WL_REGISTRY_INTERFACE) {
                        if let Some((client, global, object, handler)) =
                            client.handle_registry_request(message, &mut state.registry)
                        {
                            DispatchAction::Bind { client, global, object, handler }
                        } else {
                            continue;
                        }
                    } else {
                        let object_id = InnerObjectId {
                            id: message.sender_id,
                            serial: object.data.serial,
                            interface: object.interface,
                            client_id: client.id.clone(),
                        };
                        let opcode = message.opcode;
                        let (arguments, is_destructor, created_id) =
                            match client.process_request(&object, message) {
                                Some(args) => args,
                                None => continue,
                            };
                        // Return the whole set to invoke the callback while handle is not borrower via client
                        DispatchAction::Request {
                            object,
                            object_id,
                            opcode,
                            arguments,
                            is_destructor,
                            created_id,
                        }
                    }
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Invalid client ID",
                    ));
                }
            };
            match action {
                DispatchAction::Request {
                    object,
                    object_id,
                    opcode,
                    arguments,
                    is_destructor,
                    created_id,
                } => {
                    // temporarily unlock the state Mutex while this request is dispatched
                    std::mem::drop(state);
                    let ret = object.data.user_data.clone().request(
                        &handle.clone(),
                        data,
                        ClientId { id: client_id.clone() },
                        Message {
                            sender_id: ObjectId { id: object_id.clone() },
                            opcode,
                            args: arguments,
                        },
                    );
                    if is_destructor {
                        object.data.user_data.clone().destroyed(
                            &handle.clone(),
                            data,
                            ClientId { id: client_id.clone() },
                            ObjectId { id: object_id.clone() },
                        );
                    }
                    // acquire the lock again and continue
                    state = self.state.lock().unwrap();
                    if is_destructor {
                        if let Ok(client) = state.clients.get_client_mut(client_id.clone()) {
                            client.send_delete_id(object_id);
                        }
                    }
                    match (created_id, ret) {
                        (Some(child_id), Some(child_data)) => {
                            if let Ok(client) = state.clients.get_client_mut(client_id.clone()) {
                                client
                                    .map
                                    .with(child_id.id, |obj| obj.data.user_data = child_data)
                                    .unwrap();
                            }
                        }
                        (None, None) => {}
                        (Some(child_id), None) => {
                            // Allow the callback to not return any data if the client is already dead (typically
                            // if the callback provoked a protocol error)
                            if let Ok(client) = state.clients.get_client(client_id.clone()) {
                                if !client.killed {
                                    panic!(
                                        "Callback creating object {child_id} did not provide any object data."
                                    );
                                }
                            }
                        }
                        (None, Some(_)) => {
                            panic!("An object data was returned from a callback not creating any object");
                        }
                    }
                    // dropping the object calls destructors from which users could call into wayland-backend again.
                    // so lets release and relock the state again, to avoid a deadlock
                    std::mem::drop(state);
                    std::mem::drop(object);
                    state = self.state.lock().unwrap();
                }
                DispatchAction::Bind { object, client, global, handler } => {
                    // temporarily unlock the state Mutex while this request is dispatched
                    std::mem::drop(state);
                    let child_data = handler.bind(
                        &handle.clone(),
                        data,
                        ClientId { id: client.clone() },
                        GlobalId { id: global },
                        ObjectId { id: object.clone() },
                    );
                    // acquire the lock again and continue
                    state = self.state.lock().unwrap();
                    if let Ok(client) = state.clients.get_client_mut(client.clone()) {
                        client.map.with(object.id, |obj| obj.data.user_data = child_data).unwrap();
                    }
                }
            }
        }
        Ok(false)
    }
}

enum DispatchAction<D: 'static> {
    Request {
        object: Object<Data<D>>,
        object_id: InnerObjectId,
        opcode: u16,
        arguments: SmallVec<[Argument<ObjectId, OwnedFd>; 4]>,
        is_destructor: bool,
        created_id: Option<InnerObjectId>,
    },
    Bind {
        object: InnerObjectId,
        client: InnerClientId,
        global: InnerGlobalId,
        handler: Arc<dyn GlobalHandler<D>>,
    },
}
