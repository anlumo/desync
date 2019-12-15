// TODO: need to make it safe to drop a suspended queue (well, a suspended Desync)

use super::core::*;
use super::job::*;
use super::future_job::*;
use super::unsafe_job::*;
use super::scheduler_thread::*;
use super::job_queue::*;
use super::queue_state::*;
use super::active_queue::*;
use super::wake_thread::*;

use std::fmt;
use std::thread;
use std::sync::*;
use std::collections::vec_deque::*;

use futures::task;
use futures::task::{Context, Poll};
use futures::channel::oneshot;
use futures::future::{Future};

#[cfg(not(target_arch = "wasm32"))]
use num_cpus;

#[cfg(not(target_arch = "wasm32"))]
const MIN_THREADS: usize = 8;

lazy_static! {
    static ref SCHEDULER: Arc<Scheduler> = Arc::new(Scheduler::new());
}

///
/// The default maximum number of threads in a scheduler 
///
#[cfg(not(target_arch = "wasm32"))]
fn initial_max_threads() -> usize {
    MIN_THREADS.max(num_cpus::get()*2)
}

///
/// The default maximum number of threads in a scheduler 
///
#[cfg(target_arch = "wasm32")]
fn initial_max_threads() -> usize {
    0
}

///
/// The scheduler is used to schedule tasks onto a pool of threads
///
pub struct Scheduler {
    core: Arc<SchedulerCore>
}

impl Scheduler {
    ///
    /// Creates a new scheduler
    /// 
    /// (There's usually only one scheduler)
    /// 
    pub fn new() -> Scheduler {
        let core = SchedulerCore { 
            schedule:       Arc::new(Mutex::new(VecDeque::new())),
            threads:        Mutex::new(vec![]),
            max_threads:    Mutex::new(initial_max_threads())
        };

        Scheduler {
            core: Arc::new(core)
        }
    }

    ///
    /// Changes the maximum number of threads this scheduler can spawn (existing threads
    /// are not despawned by this method)
    ///
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_max_threads(&self, max_threads: usize) {
        // Update the maximum number of threads we can spawn
        { *self.core.max_threads.lock().expect("Max threads lock") = max_threads };

        // Schedule as many threads as we can
        while self.schedule_thread() {}
    }

    ///
    /// Changes the maximum number of threads this scheduler can spawn (existing threads
    /// are not despawned by this method)
    ///
    #[cfg(target_arch = "wasm32")]
    pub fn set_max_threads(&self, max_threads: usize) {
        // Webassembly does not support threads so we run synchronously
    }

    ///
    /// Despawns threads if we're running more than the maximum number
    /// 
    /// Must not be called from a scheduler thread (as it waits for the threads to despawn)
    ///
    pub fn despawn_threads_if_overloaded(&self) {
        let max_threads = { *self.core.max_threads.lock().expect("Max threads lock") };
        let to_despawn  = {
            // Transfer the threads from the threads vector to our _to_despawn variable
            // This is then dropped outside the mutex (so we don't block if one of the threads doesn't stop)
            let mut to_despawn  = vec![];
            let mut threads     = self.core.threads.lock().expect("Scheduler threads lock");

            while threads.len() > max_threads {
                to_despawn.push(threads.pop().expect("Missing threads").1.despawn());
            }

            to_despawn
        };

        // Wait for the threads to despawn
        to_despawn.into_iter().for_each(|join_handle| { join_handle.join().ok(); });
    }

    ///
    /// Wakes a thread to run a dormant queue. Returns true if a thread was woken up
    ///
    fn schedule_thread(&self) -> bool {
        self.core.schedule_thread(Arc::clone(&self.core))
    }

    ///
    /// If a queue is idle and has pending jobs, places it in the schedule
    ///
    fn reschedule_queue(&self, queue: &Arc<JobQueue>) {
        self.core.reschedule_queue(queue, Arc::clone(&self.core))
    }

    ///
    /// Spawns a thread in this scheduler
    ///
    pub fn spawn_thread(&self) {
        let is_busy     = Arc::new(Mutex::new(false));
        let new_thread  = SchedulerThread::new();
        self.core.threads.lock().expect("Scheduler threads lock").push((is_busy, new_thread));
    }

    ///
    /// Creates a new job queue for this scheduler
    ///
    pub fn create_job_queue(&self) -> Arc<JobQueue> {
        let new_queue = Arc::new(JobQueue::new());
        new_queue
    }

    ///
    /// Schedules a job on this scheduler, which will run after any jobs that are already 
    /// in the specified queue and as soon as a thread is available to run it.
    ///
    #[inline]
    #[deprecated(since="0.3.0", note="please use `desync` instead")]
    pub fn r#async<TFn: 'static+Send+FnOnce() -> ()>(&self, queue: &Arc<JobQueue>, job: TFn) {
        self.desync(queue, job)
    }

    ///
    /// Schedules a job on this scheduler, which will run after any jobs that are already 
    /// in the specified queue and as soon as a thread is available to run it.
    ///
    pub fn desync<TFn: 'static+Send+FnOnce() -> ()>(&self, queue: &Arc<JobQueue>, job: TFn) {
        self.schedule_job_desync(queue, Box::new(Job::new(job)));
    }

    ///
    /// Schedules a job on this scheduler, which will run after any jobs that are already 
    /// in the specified queue and as soon as a thread is available to run it.
    ///
    fn schedule_job_desync(&self, queue: &Arc<JobQueue>, job: Box<dyn ScheduledJob>) {
        enum ScheduleState {
            Idle,
            Running,
            Panicked
        }

        let schedule_queue = {
            let mut core    = queue.core.lock().expect("JobQueue core lock");

            // Push the job onto the queue
            core.queue.push_back(job);

            match core.state {
                QueueState::Idle => {
                    // If the queue is idle, then move it to pending
                    core.state = QueueState::Pending;
                    ScheduleState::Idle
                },

                QueueState::Panicked => ScheduleState::Panicked,

                _=> {
                    // If the queue is in any other state, then we leave it alone
                    ScheduleState::Running
                }
            }
        };

        // If when we were queuing the jobs we found that the queue was idle, then move it to the pending list
        match schedule_queue {
            ScheduleState::Idle => {
                // Add the queue to the schedule
                self.core.schedule.lock().expect("Schedule lock").push_back(queue.clone());

                // Wake up a thread to run it if we can
                self.schedule_thread();
            },

            ScheduleState::Running => { }

            ScheduleState::Panicked => {
                panic!("Cannot schedule jobs on a panicked queue");
            },
        }
    }

    ///
    /// Schedules a job to run and returns a future for retrieving the result
    ///
    pub fn future<TFn, TFuture>(&self, queue: &Arc<JobQueue>, job: TFn) -> impl Future<Output=Result<TFuture::Output, oneshot::Canceled>>+Send
    where   TFn:                'static+Send+FnOnce() -> TFuture,
            TFuture:            'static+Send+Future,
            TFuture::Output:    Send {
        let (send, receive) = oneshot::channel();

        let perform_job = FutureJob::new(move || {
            // Create the job when we're queued up
            let job = job();

            async {
                // Run the future
                let val = job.await;

                // Send to the channel
                send.send(val).ok();
            }
        });

        // Schedule the job
        self.schedule_job_desync(queue, Box::new(perform_job));

        // Receive channel will be notified when the job is completed
        receive
    }

    ///
    /// Pauses a queue until a particular future has completed, before performing a
    /// task with the result of that future
    ///
    pub fn after<TFn, Res: 'static+Send, Fut: 'static+Future+Send>(&self, queue: &Arc<JobQueue>, after: Fut, job: TFn) -> impl Future<Output=Result<Res, oneshot::Canceled>>+Send 
    where TFn: 'static+Send+FnOnce(Fut::Output) -> Res {
        let (send, receive) = oneshot::channel();

        // Create a future that will perform the job
        let perform_job = FutureJob::new(move || { async {
                // Wait for the task to complete
                let val = after.await;

                // Generate the result
                let result = job(val);

                // Signal the channel
                send.send(result).ok();
            }
        });

        // Add to the queue
        self.schedule_job_desync(queue, Box::new(perform_job));

        // The receive channel is the future we generated
        receive
    }

    ///
    /// Requests that a queue be suspended once it has finished all of its active jobs
    ///
    pub fn suspend(&self, queue: &Arc<JobQueue>) -> impl Future<Output=Result<(), oneshot::Canceled>>+Send {
        let (suspended, will_be_suspended)  = oneshot::channel();
        let to_suspend                      = queue.clone();

        self.desync(queue, move || {
            // Mark the queue as suspending
            let mut core = to_suspend.core.lock().expect("JobQueue core lock");

            debug_assert!(core.state == QueueState::Running);

            // Only actually suspend the core if it hasn't already been resumed elsewhere
            core.suspension_count += 1;
            if core.suspension_count == 1 {
                core.state = QueueState::Suspending;
            }

            // If we suspended, then notify the future (it'll cancel if we don't actually suspend)
            if core.suspension_count > 0 {
                suspended.send(()).ok();
            }
        });

        will_be_suspended
    }

    ///
    /// Resumes a queue that was previously suspended
    ///
    pub fn resume(&self, queue: &Arc<JobQueue>) {
        // Reduce the amount of suspension used by a queue
        // TODO: this is currently fairly unsafe as we can call resume extra times or not at all
        // TODO: better might be to return a token from suspend that we can use to resume the queue (problem is: rescheduling in the right place)
        let needs_reschedule = {
            let mut core = queue.core.lock().expect("JobQueue core lock");

            // Queue becomes less suspended
            core.suspension_count -= 1;
            if core.suspension_count <= 0 {
                match core.state {
                    QueueState::Suspended => {
                        // If the queue was suspended and should no longer be, return it to the idle state
                        core.state = QueueState::Idle;
                        true
                    },
                    QueueState::Suspending => {
                        // If the queue was in the process of suspending, cancel that
                        // and resume running
                        core.state = QueueState::Running;
                        false
                    },
                    _ => false
                }
            } else {
                false
            }
        };

        if needs_reschedule {
            self.reschedule_queue(queue);
        }
    }

    ///
    /// Runs a sync job immediately on the current thread. Queue must be in Running mode for this to be valid
    ///
    fn sync_immediate<Result, TFn: FnOnce() -> Result>(&self, queue: &Arc<JobQueue>, job: TFn) -> Result {
        debug_assert!(queue.core.lock().expect("JobQueue core lock").state == QueueState::Running);

        // Set the queue as active
        let _active = ActiveQueue { queue: &*queue };

        // Call the function to get the result
        let result = job();

        // Queue is now idle
        queue.core.lock().expect("JobQueue core lock").state = QueueState::Idle;

        // Not running any more
        self.reschedule_queue(queue);

        result
    }

    ///
    /// Runs a sync job immediately by running all the jobs in the current queue 
    ///
    fn sync_drain<Result: Send, TFn: Send+FnOnce() -> Result>(&self, queue: &Arc<JobQueue>, job: TFn) -> Result {
        debug_assert!(queue.core.lock().expect("JobQueue core lock").state == QueueState::Running);

        // Set the queue as active
        let _active = ActiveQueue { queue: &*queue };

        // When the task runs on the queue, we'll put it here
        let result = Arc::new((Mutex::new(None), Condvar::new()));

        // Queue a job that'll run the requested job and then set the result
        // We'll unpark the thread in case we need to handle a suspension
        let queue_result        = result.clone();
        let result_job          = Box::new(Job::new(move || {
            let job_result = job();
            *queue_result.0.lock().expect("Sync queue result lock") = Some(job_result);
            queue_result.1.notify_one();
        }));

        // Stuff on the queue normally has a 'static lifetime. When we're running
        // sync, the task will be done by the time this method is finished, so
        // we use an unsafe job to bypass the normal lifetime checking
        let unsafe_result_job   = UnsafeJob::new(&*result_job);
        queue.core.lock().expect("JobQueue core lock").queue.push_back(Box::new(unsafe_result_job));

        // While there is no result, run a job from the queue
        while result.0.lock().expect("Sync queue result lock").is_none() {
            if let Some(mut job) = queue.dequeue() {
                // Queue is running
                debug_assert!(queue.core.lock().unwrap().state == QueueState::Running);

                let waker       = Arc::new(WakeThread(Arc::clone(queue), thread::current()));
                let waker       = task::waker_ref(&waker);
                let mut context = Context::from_waker(&waker);

                loop {
                    let poll_result = job.run(&mut context);

                    match poll_result {
                        // A ready result ends the loop
                        Poll::Ready(()) => break,
                        Poll::Pending   => {
                            // Try to move to the parking state
                            let should_park = {
                                let mut core = queue.core.lock().unwrap();

                                core.state = match core.state {
                                    QueueState::AwokenWhileRunning  => QueueState::Running,
                                    QueueState::Running             => QueueState::WaitingForWake,
                                    other                           => panic!("Queue was in unexpected state {:?}", other)
                                };

                                core.state == QueueState::WaitingForWake
                            };

                            // Park until the queue state returns changes
                            if should_park {
                                // If should_park is set to false, the queue was awoken very quickly
                                loop {
                                    let current_state = { queue.core.lock().unwrap().state };
                                    match current_state {
                                        QueueState::Idle            => break,
                                        QueueState::WaitingForWake  => (),
                                        other                       => panic!("Queue was in unexpected state {:?}", other)
                                    }

                                    // Park until we're awoken from the other thread (once awoken, we re-check the state)
                                    thread::park();
                                }
                            }
                        }
                    }
                }
            } else {
                // Queue may have suspended (or gone to suspending and back to running)
                let wait_in_background = {
                    let mut core = queue.core.lock().expect("JobQueue core lock");
                    if core.state == QueueState::Suspending {
                        // Finish suspension, then wait for job to complete
                        core.state = QueueState::Suspended;
                        true
                    } else {
                        // Queue is still running
                        debug_assert!(core.state == QueueState::Running);
                        false
                    }
                };

                if wait_in_background {
                    // After we ran the thread, it suspended. It will be rescheduled in the background before it runs.
                    while result.0.lock().expect("Sync queue result lock").is_none() {
                        // Park until the result becomes available
                        let parking = &result.1;
                        let result  = result.0.lock().unwrap();
                        let _result = parking.wait(result).unwrap();
                    }
                }
            }
        }

        // Reschedule the queue if there are any events left pending
        // Note: the queue is already pending when we start running events from it here.
        // This means it'll get dequeued by a thread eventually: maybe while it's running
        // here. As we've set the queue state to running while we're busy, the thread won't
        // start the queue while it's already running.
        queue.core.lock().expect("JobQueue core lock").state = QueueState::Idle;
        self.reschedule_queue(queue);

        // Get the final result by swapping it out of the mutex
        let mut old_result      = result.0.lock().expect("Sync queue result lock");
        let final_result        = old_result.take();

        final_result.expect("Finished sync request without result")
    }

    ///
    /// Queues a sync job and waits for the queue to finish running 
    ///
    fn sync_background<Result: Send, TFn: Send+FnOnce() -> Result>(&self, queue: &Arc<JobQueue>, job: TFn) -> Result {
        // Queue a job that unparks this thread when done
        let pair    = Arc::new((Mutex::new(None), Condvar::new()));
        let pair2   = pair.clone();

        // Safe job that signals the condvar when needed
        let job     = Box::new(Job::new(move || {
            let &(ref result, ref cvar) = &*pair2;

            // Run the job
            let actual_result = job();

            // Set the result and notify the waiting thread
            *result.lock().expect("Background job result lock") = Some(actual_result);
            cvar.notify_one();
        }));
        
        // Unsafe job with unbounded lifetime is needed because stuff on the queue normally needs a static lifetime
        let need_reschedule = {
            // Schedule the job and see if the queue went back to 'idle'. Reschedule if it is.
            let unsafe_job  = Box::new(UnsafeJob::new(&*job));
            let mut core    = queue.core.lock().expect("JobQueue core lock");

            core.queue.push_back(unsafe_job);
            core.state == QueueState::Idle
        };
        if need_reschedule { self.reschedule_queue(queue); }

        // Wait for the result to arrive (and the sweet relief of no more unsafe job)
        let &(ref lock, ref cvar) = &*pair;
        let mut result = lock.lock().expect("Background job result lock");
        
        while result.is_none() {
            result = cvar.wait(result).expect("Background job cvar wait");
        }

        // Get the final result by swapping it out of the mutex
        let final_result        = result.take();
        final_result.expect("Finished background sync job without result")
    }

    ///
    /// Schedules a job on this scheduler, which will run after any jobs that are already
    /// in the specified queue. This function will not return until the job has completed.
    ///
    pub fn sync<Result: Send, TFn: Send+FnOnce() -> Result>(&self, queue: &Arc<JobQueue>, job: TFn) -> Result {
        enum RunAction {
            /// The queue is empty: call the function directly and don't bother with storing a result
            Immediate,

            /// The queue is not empty but not running: drain on this thread so we get to the sync op
            DrainOnThisThread,

            /// The queue is running in the background
            WaitForBackground,

            /// The queue is panicked
            Panic
        }

        // If the queue is idle when this is called, we need to schedule this task on this thread rather than one owned by the background process
        let run_action = {
            let mut core = queue.core.lock().expect("JobQueue core lock");

            match core.state {
                QueueState::Suspended           => RunAction::WaitForBackground,
                QueueState::Suspending          => RunAction::WaitForBackground,
                QueueState::Running             => RunAction::WaitForBackground,
                QueueState::WaitingForWake      => RunAction::WaitForBackground,
                QueueState::AwokenWhileRunning  => RunAction::WaitForBackground,
                QueueState::Panicked            => RunAction::Panic,
                QueueState::Pending             => { core.state = QueueState::Running; RunAction::DrainOnThisThread },
                QueueState::Idle                => { core.state = QueueState::Running; RunAction::Immediate }
            }
        };

        match run_action {
            RunAction::Immediate            => self.sync_immediate(queue, job),
            RunAction::DrainOnThisThread    => self.sync_drain(queue, job),
            RunAction::WaitForBackground    => self.sync_background(queue, job),
            RunAction::Panic                => panic!("Cannot schedule new jobs on a panicked queue")
        }
    }
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let threads = {
            let threads         = self.core.threads.lock().expect("Scheduler threads lock");
            let busyness:String = threads.iter().map(|&(ref busy, _)| { if *busy.lock().expect("Thread busy lock") { 'B' } else { 'I' } }).collect();

            busyness
        };
        let queue_size = format!("Pending queue count: {}", self.core.schedule.lock().expect("Schedule lock").len());

        fmt.write_str(&format!("{} {}", threads, queue_size))
    }
}

///
/// Retrieves the global scheduler
///
pub fn scheduler<'a>() -> &'a Scheduler {
    &SCHEDULER
}

///
/// Creates a scheduler queue
///
pub fn queue() -> Arc<JobQueue> {
    scheduler().create_job_queue()
}

///
/// Performs an action asynchronously on the specified queue
///
#[inline]
#[deprecated(since="0.3.0", note="please use `desync` instead")]
pub fn r#async<TFn: 'static+Send+FnOnce() -> ()>(queue: &Arc<JobQueue>, job: TFn) {
    desync(queue, job)
}

///
/// Performs an action asynchronously on the specified queue
///
pub fn desync<TFn: 'static+Send+FnOnce() -> ()>(queue: &Arc<JobQueue>, job: TFn) {
    scheduler().desync(queue, job)
}

///
/// Schedules a job to run and returns a future for retrieving the result
///
pub fn future<TFn, TFuture>(queue: &Arc<JobQueue>, job: TFn) -> impl Future<Output=Result<TFuture::Output, oneshot::Canceled>>+Send
where   TFn:                'static+Send+FnOnce() -> TFuture,
        TFuture:            'static+Send+Future,
        TFuture::Output:    Send {
    scheduler().future(queue, job)
}

///
/// Performs an action synchronously on the specified queue 
///
pub fn sync<Result: Send, TFn: Send+FnOnce() -> Result>(queue: &Arc<JobQueue>, job: TFn) -> Result {
    scheduler().sync(queue, job)
}