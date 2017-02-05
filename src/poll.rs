use {sys, Evented, Token};
use event::{self, Ready, Event, PollOpt};
use std::{fmt, io, mem, ptr, usize};
use std::cell::{UnsafeCell, Cell};
use std::{marker, ops, isize};
use std::sync::Arc;
use std::sync::atomic::{self, AtomicUsize, AtomicPtr, AtomicBool};
use std::sync::atomic::Ordering::{self, Acquire, Release, AcqRel};
use std::time::Duration;

/// Polls for readiness events on all registered values.
///
/// The `Poll` type acts as an interface allowing a program to wait on a set of
/// IO handles until one or more become "ready" to be operated on. An IO handle
/// is considered ready to operate on when the given operation can complete
/// without blocking.
///
/// To use `Poll`, an IO handle must first be registered with the `Poll`
/// instance using the `register()` handle. An `Ready` representing the
/// program's interest in the socket is specified as well as an arbitrary
/// `Token` which is used to identify the IO handle in the future.
///
/// ## Edge-triggered and level-triggered
///
/// An IO handle registration may request edge-triggered notifications or
/// level-triggered notifications. This is done by specifying the `PollOpt`
/// argument to `register()` and `reregister()`.
///
/// ## Portability
///
/// Cross platform portability is provided for Mio's TCP & UDP implementations.
///
/// ## Examples
///
/// ```no_run
/// use mio::*;
/// use mio::tcp::*;
///
/// // Construct a new `Poll` handle as well as the `Events` we'll store into
/// let poll = Poll::new().unwrap();
/// let mut events = Events::with_capacity(1024);
///
/// // Connect the stream
/// let stream = TcpStream::connect(&"173.194.33.80:80".parse().unwrap()).unwrap();
///
/// // Register the stream with `Poll`
/// poll.register(&stream, Token(0), Ready::all(), PollOpt::edge()).unwrap();
///
/// // Wait for the socket to become ready
/// poll.poll(&mut events, None).unwrap();
/// ```
pub struct Poll {
    // This type is `Send`, but not `Sync`, so ensure it's exposed as such.
    _marker: marker::PhantomData<Cell<()>>,

    // Platform specific IO selector
    selector: sys::Selector,

    // Custom readiness queue
    readiness_queue: ReadinessQueue,
}

/// Handle to a Poll registration. Used for registering custom types for event
/// notifications.
pub struct Registration {
    inner: RegistrationInner,
}

/// Used to update readiness for an associated `Registration`. `SetReadiness`
/// is `Sync` which allows it to be updated across threads.
#[derive(Clone)]
pub struct SetReadiness {
    inner: RegistrationInner,
}

struct RegistrationInner {
    // ARC pointer to the Poll's readiness queue
    queue: ReadinessQueue,

    // Unsafe pointer to the registration's node. The node is owned by the
    // registration queue.
    node: *mut ReadinessNode,
}

#[derive(Clone)]
struct ReadinessQueue {
    inner: Arc<UnsafeCell<ReadinessQueueInner>>,
}

struct ReadinessQueueInner {
    // Used to wake up `Poll` when readiness is set in another thread.
    awakener: sys::Awakener,

    // linked list of nodes that are pending some processing
    head_readiness: AtomicPtr<ReadinessNode>,

    // Tail of readiness queue, used to pop nodes off
    //
    // Only accessed by Poll::poll
    tail_readiness: *mut ReadinessNode,

    // Fake readiness node used to punctuate the end of the readiness queue.
    // Before attempting to read from the queue, this node is inserted in order
    // to partition the queue between nodes that are "owened" by the dequeue end
    // and nodes that will be pushed on by producers.
    end_marker: Box<ReadinessNode>,

    // Similar to `end_marker`, but this node signals to producers that `Poll`
    // has gone to sleep and must be woken up.
    sleep_marker: Box<ReadinessNode>,
}

struct ReadyRef {
    ptr: *mut ReadinessNode,
}

// All node state can be compacted in a single AtomicUsize

// Concurrent calls to update are permitted, but only one will win. If
// SetReadiness is called concurrently to an update, then the update will
// requeue the node.
struct ReadinessNode {
    // Node state, see struct docs for `ReadinessState`
    state: AtomicState,

    // Three slots for setting tokens
    token_0: UnsafeCell<Token>,
    token_1: UnsafeCell<Token>,
    token_2: UnsafeCell<Token>,

    // Used when the node is queued in the readiness linked list. Accessing
    // this field requires winning the "queue" lock
    next_readiness: AtomicPtr<ReadinessNode>,

    // Update lock
    update_lock: AtomicBool,

    // Number of outstanding set readiness handles
    num_set_readiness: AtomicUsize,

    // Number of outstandin Registration nodes
    num_registration: AtomicUsize,

    // Tracks the number of `ReadyRef` pointers
    ref_count: AtomicUsize,
}

struct AtomicState {
    inner: AtomicUsize,
}

// Packs all the necessary info
//
// Ready:       4 bits
// Interest:    4 bits
// PollOpts:    4 bits
// Enqueued:    1 bit
// Dropped:     1 bit
//
// PollToken:   2 bits
// RegToken:    2 bits
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct ReadinessState(usize);

enum Dequeue {
    Data(*mut ReadinessNode),
    Empty,
    Inconsistent,
}

const AWAKEN: Token = Token(usize::MAX);
const MAX_REFCOUNT: usize = (isize::MAX) as usize;

/*
 *
 * ===== Poll =====
 *
 */

impl Poll {
    /// Return a new `Poll` handle using a default configuration.
    pub fn new() -> io::Result<Poll> {
        let poll = Poll {
            selector: try!(sys::Selector::new()),
            readiness_queue: try!(ReadinessQueue::new()),
            _marker: marker::PhantomData,
        };

        // Register the notification wakeup FD with the IO poller
        try!(poll.readiness_queue.inner().awakener.register(&poll, AWAKEN, Ready::readable(), PollOpt::edge()));

        Ok(poll)
    }

    /// Register an `Evented` handle with the `Poll` instance.
    pub fn register<E: ?Sized>(&self, io: &E, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        try!(validate_args(token, interest));

        /*
         * Undefined behavior:
         * - Reusing a token with a different `Evented` without deregistering
         * (or closing) the original `Evented`.
         */
        trace!("registering with poller");

        // Register interests for this socket
        try!(io.register(self, token, interest, opts));

        Ok(())
    }

    /// Re-register an `Evented` handle with the `Poll` instance.
    pub fn reregister<E: ?Sized>(&self, io: &E, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        try!(validate_args(token, interest));

        trace!("registering with poller");

        // Register interests for this socket
        try!(io.reregister(self, token, interest, opts));

        Ok(())
    }

    /// Deregister an `Evented` handle with the `Poll` instance.
    pub fn deregister<E: ?Sized>(&self, io: &E) -> io::Result<()>
        where E: Evented
    {
        trace!("deregistering IO with poller");

        // Deregister interests for this socket
        try!(io.deregister(self));

        Ok(())
    }

    /// Block the current thread and wait until any `Evented` values registered
    /// with the `Poll` instance are ready or the given timeout has elapsed.
    pub fn poll(&self,
                events: &mut Events,
                timeout: Option<Duration>) -> io::Result<usize> {
        let timeout = if !self.readiness_queue.is_empty() {
            trace!("custom readiness queue has pending events");
            // Never block if the readiness queue has pending events
            Some(Duration::from_millis(0))
        } else if !self.readiness_queue.prepare_for_sleep() {
            Some(Duration::from_millis(0))
        } else {
            timeout
        };

        // First get selector events
        let awoken = try!(self.selector.select(&mut events.inner, AWAKEN,
                                               timeout));

        if awoken {
            self.readiness_queue.inner().awakener.cleanup();
        }

        // Poll custom event queue
        self.readiness_queue.poll(&mut events.inner);

        // Return number of polled events
        Ok(events.len())
    }
}

fn validate_args(token: Token, interest: Ready) -> io::Result<()> {
    if token == AWAKEN {
        return Err(io::Error::new(io::ErrorKind::Other, "invalid token"));
    }

    if !interest.is_readable() && !interest.is_writable() {
        return Err(io::Error::new(io::ErrorKind::Other, "interest must include readable or writable"));
    }

    Ok(())
}

impl fmt::Debug for Poll {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Poll")
    }
}

/// A buffer for I/O events to get placed into, passed to `Poll::poll`.
///
/// This structure is normally re-used on each turn of the event loop and will
/// contain any I/O events that happen during a `poll`. After a call to `poll`
/// returns the various accessor methods on this structure can be used to
/// iterate over the underlying events that ocurred.
pub struct Events {
    inner: sys::Events,
}

/// Iterate an Events structure
pub struct EventsIter<'a> {
    inner: &'a Events,
    pos: usize,
}

impl Events {
    /// Create a net blank set of events capable of holding up to `capacity`
    /// events.
    ///
    /// This parameter typically is an indicator on how many events can be
    /// returned each turn of the event loop, but it is not necessarily a hard
    /// limit across platforms.
    pub fn with_capacity(capacity: usize) -> Events {
        Events {
            inner: sys::Events::with_capacity(capacity),
        }
    }

    /// Returns the `idx`-th event.
    ///
    /// Returns `None` if `idx` is greater than the length of this event buffer.
    pub fn get(&self, idx: usize) -> Option<Event> {
        self.inner.get(idx)
    }

    /// Returns how many events this buffer contains.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns whether this buffer contains 0 events.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn iter(&self) -> EventsIter {
        EventsIter {
            inner: self,
            pos: 0
        }
    }
}

impl<'a> IntoIterator for &'a Events {
    type Item = Event;
    type IntoIter = EventsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> Iterator for EventsIter<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        let ret = self.inner.get(self.pos);
        self.pos += 1;
        ret
    }
}

// ===== Accessors for internal usage =====

pub fn selector(poll: &Poll) -> &sys::Selector {
    &poll.selector
}

/*
 *
 * ===== Registration =====
 *
 */

impl Registration {
    /// Create a new `Registration` associated with the given `Poll` instance.
    /// The returned `Registration` will be associated with this `Poll` for its
    /// entire lifetime.
    pub fn new(poll: &Poll, token: Token, interest: Ready, opt: PollOpt) -> (Registration, SetReadiness) {
        // Clone handle to the readiness queue
        let queue = poll.readiness_queue.clone();

        // Allocate the registration node. The new node will have `ref_count`
        // set to 3, `num_set_readiness` set to 1, and `num_registration` set to
        // 1. The 3 ref_counts represent ownership by one SetReadiness, one
        // Registration, and the Poll handle.

        let node = Box::into_raw(Box::new(ReadinessNode::new(token, interest, opt)));

        let registration = Registration {
            inner: RegistrationInner {
                node: node,
                queue: queue.clone(),
            },
        };

        let set_readiness = SetReadiness {
            inner: RegistrationInner {
                node: node,
                queue: queue.clone(),
            },
        };

        (registration, set_readiness)
    }

    pub fn update(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.inner.update(poll, token, interest, opts)
    }

    pub fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.inner.update(poll, Token(0), Ready::none(), PollOpt::empty())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let n = self.inner.num_registration.fetch_sub(1, AcqRel);

        if n == 1 {
            // TODO: Flag the node as dropped
            unimplemented!();
        }
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Registration")
            .finish()
    }
}

unsafe impl Send for Registration { }

impl SetReadiness {
    pub fn readiness(&self) -> Ready {
        self.inner.readiness()
    }

    pub fn set_readiness(&self, ready: Ready) -> io::Result<()> {
        self.inner.set_readiness(ready)
    }
}

impl Drop for SetReadiness {
    fn drop(&mut self) {
        let n = self.inner.num_set_readiness.fetch_sub(1, AcqRel);

        if n == 1 {
            // TODO: flag node as dropped
            unimplemented!();
        }
    }
}

unsafe impl Send for SetReadiness { }
unsafe impl Sync for SetReadiness { }

impl RegistrationInner {
    /// Get the registration's readiness.
    fn readiness(&self) -> Ready {
        self.state.load(Acquire).readiness()
    }

    /// Set the registration's readiness.
    ///
    /// This function can be called concurrently by an arbitrary number of
    /// SetReadiness handles.
    fn set_readiness(&self, ready: Ready) -> io::Result<()> {
        let mut curr = self.state.load(Acquire);
        let mut queue = false;

        loop {
            let mut next = curr;

            // Update the readiness
            next.set_readiness(ready);

            // If the readiness is not blank, try to obtain permission to
            // push the node into the readiness queue.
            if ready.is_some() {
                queue = next.set_queued();
            }

            let actual = self.state.compare_and_swap(curr, next, AcqRel);

            if curr == actual {
                break;
            }

            curr = actual;
        }

        if queue {
            if self.queue.enqueue_node(self) {
                try!(self.queue.wakeup());
            }
        }

        Ok(())
    }

    fn update(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        unimplemented!();
        /*
        // Update the registration data
        try!(self.registration_data_mut(&poll.readiness_queue)).update(token, interest, opts);

        // If the node is currently ready, re-queue?
        if !event::is_empty(self.readiness()) {
            // The releaxed ordering of `self.readiness()` is sufficient here.
            // All mutations to readiness will immediately attempt to queue the
            // node for processing. This means that this call to
            // `queue_for_processing` is only intended to handle cases where
            // the node was dequeued in `poll` and then has the interest
            // changed, which means that the "newest" readiness value is
            // already known by the current thread.
            let needs_wakeup = self.queue_for_processing();
            debug_assert!(!needs_wakeup, "something funky is going on");
        }

        Ok(())
        */
    }
}

impl ops::Deref for RegistrationInner {
    type Target = ReadinessNode;

    fn deref(&self) -> &ReadinessNode {
        unsafe { &*self.node }
    }
}

impl Clone for RegistrationInner {
    fn clone(&self) -> RegistrationInner {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.ref_count.fetch_add(1, Ordering::Relaxed);

        // However we need to guard against massive refcounts in case someone
        // is `mem::forget`ing Arcs. If we don't do this the count can overflow
        // and users will use-after free. We racily saturate to `isize::MAX` on
        // the assumption that there aren't ~2 billion threads incrementing
        // the reference count at once. This branch will never be taken in
        // any realistic program.
        //
        // We abort because such a program is incredibly degenerate, and we
        // don't care to support it.
        if old_size & !MAX_REFCOUNT != 0 {
            // TODO: This should really abort the process
            panic!();
        }

        RegistrationInner {
            queue: self.queue.clone(),
            node: self.node.clone(),
        }
    }
}

impl Drop for RegistrationInner {
    fn drop(&mut self) {
        release_node(self.node);
    }
}

/*
 *
 * ===== ReadinessQueue =====
 *
 */

impl ReadinessQueue {
    fn new() -> io::Result<ReadinessQueue> {
        let end_marker = Box::new(ReadinessNode::marker());
        let sleep_marker = Box::new(ReadinessNode::marker());

        let ptr = &*end_marker as *const _ as *mut _;

        Ok(ReadinessQueue {
            inner: Arc::new(UnsafeCell::new(ReadinessQueueInner {
                awakener: try!(sys::Awakener::new()),
                head_readiness: AtomicPtr::new(ptr),
                tail_readiness: ptr,
                end_marker: end_marker,
                sleep_marker: sleep_marker,
            }))
        })
    }

    fn poll(&self, dst: &mut sys::Events) {
        unimplemented!();
        /*
        let ready = self.take_ready();

        // TODO: Cap number of nodes processed
        for node in ready {
            let mut events;
            let opts;

            {
                let node_ref = node.as_ref().unwrap();
                opts = node_ref.poll_opts();

                // Atomically read queued. Use Acquire ordering to set a
                // barrier before reading events, which will be read using
                // `Relaxed` ordering. Reading events w/ `Relaxed` is OK thanks to
                // the acquire / release hand off on `queued`.
                let mut queued = node_ref.queued.load(Ordering::Acquire);
                events = node_ref.poll_events();

                // Enter a loop attempting to unset the "queued" bit or requeuing
                // the node.
                loop {
                    // In the following conditions, the registration is removed from
                    // the readiness queue:
                    //
                    // - The registration is edge triggered.
                    // - The event set contains no events
                    // - There is a requested delay that has not already expired.
                    //
                    // If the drop flag is set though, the node is never queued
                    // again.
                    if event::is_drop(events) {
                        // dropped nodes are always processed immediately. There is
                        // also no need to unset the queued bit as the node should
                        // not change anymore.
                        break;
                    } else if opts.is_edge() || event::is_empty(events) {
                        // An acquire barrier is set in order to re-read the
                        // `events field. `Release` is not needed as we have not
                        // mutated any field that we need to expose to the producer
                        // thread.
                        let next = node_ref.queued.compare_and_swap(queued, 0, Ordering::Acquire);

                        // Re-read in order to ensure we have the latest value
                        // after having marked the registration has dequeued from
                        // the readiness queue. Again, `Relaxed` is OK since we set
                        // the barrier above.
                        events = node_ref.poll_events();

                        if queued == next {
                            break;
                        }

                        queued = next;
                    } else {
                        // The node needs to stay queued for readiness, so it gets
                        // pushed back onto the queue.
                        //
                        // TODO: It would be better to build up a batch list that
                        // requires a single CAS. Also, `Relaxed` ordering would be
                        // OK here as the prepend only needs to be visible by the
                        // current thread.
                        let needs_wakeup = self.prepend_readiness_node(node.clone());
                        debug_assert!(!needs_wakeup, "something funky is going on");
                        break;
                    }
                }
            }

            // Process the node.
            if event::is_drop(events) {
                // Release the node
                let _ = self.unlink_node(node);
            } else if !events.is_none() {
                let node_ref = node.as_ref().unwrap();

                // TODO: Don't push the event if the capacity of `dst` has
                // been reached
                trace!("returning readiness event {:?} {:?}", events,
                       node_ref.token());
                dst.push_event(Event::new(events, node_ref.token()));

                // If one-shot, disarm the node
                if opts.is_oneshot() {
                    node_ref.registration_data_mut().disable();
                }
            }
        }
        */
    }

    fn wakeup(&self) -> io::Result<()> {
        self.inner().awakener.wakeup()
    }

    // Attempts to state to sleeping. This involves changing `head_readiness`
    // to `sleep_token`. Returns true if `poll` can sleep.
    fn prepare_for_sleep(&self) -> bool {
        unimplemented!();
        /*
        // Use relaxed as no memory besides the pointer is being sent across
        // threads. Ordering doesn't matter, only the current value of
        // `head_readiness`.
        ptr::null_mut() == self.inner().head_readiness
            .compare_and_swap(ptr::null_mut(), self.sleep_token(), Ordering::Relaxed)
            */
    }

    /// Prepend the given node to the head of the readiness queue. This is done
    /// with relaxed ordering. Returns true if `Poll` needs to be woken up.
    fn enqueue_node(&self, node: &ReadinessNode) -> bool {
        let inner = self.inner();
        let node_ptr = node as *const ReadinessNode as *mut ReadinessNode;

        debug_assert!(node.next_readiness.load(Ordering::Relaxed).is_null());

        unsafe {
            let prev = inner.head_readiness.swap(node_ptr, AcqRel);
            (*prev).next_readiness.store(node_ptr, Release);

            prev == self.sleep_marker()
        }
    }

    unsafe fn dequeue_node(&self) -> Dequeue {
        unimplemented!();
        /*
        let tail = *self.tail_readiness.get();
        let next = (*tail).next.load(Acquire);

        if !next.is_null() {
            *self.tail.get() = next;

            // The relaxed ordering here is acceptable as the "queued" flag
            // guards any access to the node

            /*
            assert!((*tail).value.is_none());
            assert!((*next).value.is_some());
            let ret = (*next).value.take().unwrap();

            return Dequeue::Data(ret);
            */
            unimplemented!();
        }

        if self.head_readiness.load(Ordering::Acquire) == tail {
            Dequeue::Empty
        } else {
            Dequeue::Inconsistent
        }
        */
    }

    fn end_marker(&self) -> *mut ReadinessNode {
        &*self.inner().end_marker as *const ReadinessNode as *mut ReadinessNode
    }

    fn sleep_marker(&self) -> *mut ReadinessNode {
        &*self.inner().sleep_marker as *const ReadinessNode as *mut ReadinessNode
    }

    fn identical(&self, other: &ReadinessQueue) -> bool {
        self.inner.get() == other.inner.get()
    }

    fn is_empty(&self) -> bool {
        unimplemented!();
    }

    fn inner(&self) -> &ReadinessQueueInner {
        unsafe { &*self.inner.get() }
    }
}

/*
unsafe impl Send for ReadinessQueue { }
*/

impl ReadinessNode {
    /// Return a new `ReadinessNode`, initialized with a ref_count of 3.
    fn new(token: Token, interest: Ready, opt: PollOpt) -> ReadinessNode {
        ReadinessNode {
            state: AtomicState::new(interest, opt),
            // Only the first token is set, the others are initialized to 0
            token_0: UnsafeCell::new(token),
            token_1: UnsafeCell::new(Token(0)),
            token_2: UnsafeCell::new(Token(1)),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            update_lock: AtomicBool::new(false),
            num_set_readiness: AtomicUsize::new(1),
            num_registration: AtomicUsize::new(1),
            ref_count: AtomicUsize::new(3),
        }
    }

    fn marker() -> ReadinessNode {
        ReadinessNode {
            state: AtomicState::new(Ready::none(), PollOpt::empty()),
            token_0: UnsafeCell::new(Token(0)),
            token_1: UnsafeCell::new(Token(0)),
            token_2: UnsafeCell::new(Token(0)),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            update_lock: AtomicBool::new(false),
            num_set_readiness: AtomicUsize::new(0),
            num_registration: AtomicUsize::new(0),
            ref_count: AtomicUsize::new(0),
        }
    }
}

fn release_node(ptr: *mut ReadinessNode) {
    unsafe {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object. This
        // same logic applies to the below `fetch_sub` to the `weak` count.
        if (*ptr).ref_count.fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        atomic::fence(Acquire);

        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
}

impl AtomicState {
    fn new(interest: Ready, opt: PollOpt) -> AtomicState {
        let state = ReadinessState::new(interest, opt);

        AtomicState {
            inner: AtomicUsize::new(state.into()),
        }
    }

    /// Loads the current `ReadinessState`
    fn load(&self, order: Ordering) -> ReadinessState {
        self.inner.load(order).into()
    }

    /// Stores a state if the current state is the same as `current`.
    fn compare_and_swap(&self, current: ReadinessState, new: ReadinessState, order: Ordering) -> ReadinessState {
        self.inner.compare_and_swap(current.into(), new.into(), order).into()
    }
}

const MASK_2: usize = 4 - 1;
const MASK_4: usize = 16 - 1;
const QUEUED_MASK: usize = 1 << QUEUED_SHIFT;

const INTEREST_SHIFT: usize = 4;
const POLL_OPT_SHIFT: usize = 8;
const POLL_TOKEN_SHIFT: usize = 12;
const REGISTRATION_TOKEN_SHIFT: usize = 14;
const QUEUED_SHIFT: usize = 16;

impl ReadinessState {
    // Create a `ReadinessState` initialized with the provided arguments
    fn new(interest: Ready, opt: PollOpt) -> ReadinessState {
        let interest = event::ready_as_usize(interest);
        let opt = event::opt_as_usize(opt);

        debug_assert!(interest <= MASK_4);
        debug_assert!(opt <= MASK_4);

        let mut val = interest << INTEREST_SHIFT;
        val |= opt << POLL_OPT_SHIFT;

        ReadinessState(val)
    }

    /// Get the readiness
    fn readiness(&self) -> Ready {
        event::ready_from_usize(self.0 & MASK_4)
    }

    /// Set the readiness
    fn set_readiness(&mut self, v: Ready) {
        self.0 = (self.0 & !MASK_4) |
            event::ready_as_usize(v);
    }

    /// Get the interest
    fn interest(&self) -> Ready {
        let v = self.0 >> INTEREST_SHIFT;
        event::ready_from_usize(v & MASK_4)
    }

    /// Set the interest
    fn set_interest(&mut self, v: Ready) {
        self.0 = (self.0 & !(MASK_4 << INTEREST_SHIFT)) |
            (event::ready_as_usize(v) << INTEREST_SHIFT)
    }

    /// Get the poll options
    fn poll_opt(&self) -> PollOpt {
        let v = self.0 >> POLL_OPT_SHIFT;
        event::opt_from_usize(v)
    }

    /// Set the poll options
    fn set_poll_opt(&mut self, v: PollOpt) {
        self.0 = (self.0 & !(MASK_4 << POLL_OPT_SHIFT)) |
            (event::opt_as_usize(v) << POLL_OPT_SHIFT)
    }

    fn is_queued(&mut self) -> bool {
        self.0 & QUEUED_MASK == QUEUED_MASK
    }

    /// Set the queued flag, returns true if successful
    fn set_queued(&mut self) -> bool {
        let ret = !self.is_queued();
        self.0 |= QUEUED_MASK;
        ret
    }
}

impl From<ReadinessState> for usize {
    fn from(src: ReadinessState) -> usize {
        src.0
    }
}

impl From<usize> for ReadinessState {
    fn from(src: usize) -> ReadinessState {
        ReadinessState(src)
    }
}

/*

impl ReadyRef {
    fn new(ptr: *mut ReadinessNode) -> ReadyRef {
        ReadyRef { ptr: ptr }
    }

    fn none() -> ReadyRef {
        ReadyRef { ptr: ptr::null_mut() }
    }

    fn take(&mut self) -> ReadyRef {
        let ret = ReadyRef { ptr: self.ptr };
        self.ptr = ptr::null_mut();
        ret
    }

    fn is_some(&self) -> bool {
        !self.is_none()
    }

    fn is_none(&self) -> bool {
        self.ptr.is_null()
    }

    fn as_ref(&self) -> Option<&ReadinessNode> {
        if self.ptr.is_null() {
            return None;
        }

        unsafe { Some(&*self.ptr) }
    }

    fn as_mut(&mut self) -> Option<&mut ReadinessNode> {
        if self.ptr.is_null() {
            return None;
        }

        unsafe { Some(&mut *self.ptr) }
    }
}

impl Clone for ReadyRef {
    fn clone(&self) -> ReadyRef {
        ReadyRef::new(self.ptr)
    }
}

impl fmt::Pointer for ReadyRef {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.as_ref() {
            Some(r) => fmt::Pointer::fmt(&r, fmt),
            None => fmt::Pointer::fmt(&ptr::null::<ReadinessNode>(), fmt),
        }
    }
}
*/

#[cfg(test)]
mod test {
    use {Ready, Poll, PollOpt, Registration, SetReadiness, Token, Events};
    use std::time::Duration;

    fn ensure_send<T: Send>(_: &T) {}
    fn ensure_sync<T: Sync>(_: &T) {}

    #[allow(dead_code)]
    fn ensure_type_bounds(r: &Registration, s: &SetReadiness) {
        ensure_send(r);
        ensure_send(s);
        ensure_sync(s);
    }

    fn readiness_node_count(poll: &Poll) -> usize {
        let mut cur = poll.readiness_queue.inner().head_all_nodes.as_ref();
        let mut cnt = 0;

        while let Some(node) = cur {
            cnt += 1;
            cur = node.next_all_nodes.as_ref();
        }

        cnt
    }

    #[test]
    pub fn test_nodes_do_not_leak() {
        let mut poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(1024);
        let mut registrations = Vec::with_capacity(1_000);

        for _ in 0..3 {
            registrations.push(Registration::new(&mut poll, Token(0), Ready::readable(), PollOpt::edge()));
        }

        drop(registrations);

        // Poll
        let num = poll.poll(&mut events, Some(Duration::from_millis(300))).unwrap();

        assert_eq!(0, num);
        assert_eq!(0, readiness_node_count(&poll));
    }
}
