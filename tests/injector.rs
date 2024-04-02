use std::pin::Pin;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use crossbeam_intrusion::Steal::Success;
use crossbeam_intrusion::{Injector, Worker};
use crossbeam_utils::thread::scope;
use rand::Rng;

type TaskTypes = crossbeam_intrusion::QueueTypes<Key, Arc<Task<usize>>>;

pin_project_lite::pin_project!(
    struct Task<V: ?Sized> {
        #[pin]
        intrusive: pin_queue::Intrusive<TaskTypes>,
        value: V,
    }
);

impl<V> Task<V> {
    pub fn new(value: V) -> Self {
        Self {
            intrusive: pin_queue::Intrusive::new(),
            value,
        }
    }
}

struct Key;
impl pin_queue::GetIntrusive<TaskTypes> for Key {
    fn get_intrusive(p: Pin<&Task<usize>>) -> Pin<&pin_queue::Intrusive<TaskTypes>> {
        p.project_ref().intrusive
    }
}

// #[test]
// fn smoke() {
//     let q = Injector::<Key, Arc<Task<usize>>>::new();
//     assert_eq!(q.steal(), Empty);

//     q.push(Arc::pin(Task::new(1)));
//     q.push(Arc::pin(Task::new(2)));
//     assert_eq!(q.steal(), Success(1));
//     assert_eq!(q.steal(), Success(2));
//     assert_eq!(q.steal(), Empty);

//     q.push(Arc::pin(Task::new(3)));
//     assert_eq!(q.steal(), Success(3));
//     assert_eq!(q.steal(), Empty);
// }

#[test]
fn is_empty() {
    let q = Injector::<Key, Arc<Task<usize>>>::new();
    assert!(q.is_empty());

    q.push(Arc::pin(Task::new(1)));
    assert!(!q.is_empty());
    q.push(Arc::pin(Task::new(2)));
    assert!(!q.is_empty());

    let _ = q.steal();
    assert!(!q.is_empty());
    let _ = q.steal();
    assert!(q.is_empty());

    q.push(Arc::pin(Task::new(3)));
    assert!(!q.is_empty());
    let _ = q.steal();
    assert!(q.is_empty());
}

#[test]
fn spsc() {
    #[cfg(miri)]
    const COUNT: usize = 500;
    #[cfg(not(miri))]
    const COUNT: usize = 100_000;

    let q = Injector::<Key, Arc<Task<usize>>>::new();

    scope(|scope| {
        scope.spawn(|_| {
            for i in 0..COUNT {
                loop {
                    if let Success(v) = q.steal() {
                        assert_eq!(i, v.value);
                        break;
                    }
                    #[cfg(miri)]
                    std::hint::spin_loop();
                }
            }

            assert!(q.steal().is_empty());
        });

        for i in 0..COUNT {
            q.push(Arc::pin(Task::new(i)));
        }
    })
    .unwrap();
}

#[test]
fn mpmc() {
    #[cfg(miri)]
    const COUNT: usize = 500;
    #[cfg(not(miri))]
    const COUNT: usize = 25_000;
    const THREADS: usize = 4;

    let q = Injector::<Key, Arc<Task<usize>>>::new();
    let v = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();

    scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|_| {
                for i in 0..COUNT {
                    q.push(Arc::pin(Task::new(i)));
                }
            });
        }

        for _ in 0..THREADS {
            scope.spawn(|_| {
                for _ in 0..COUNT {
                    loop {
                        if let Success(n) = q.steal() {
                            v[n.value].fetch_add(1, SeqCst);
                            break;
                        }
                        #[cfg(miri)]
                        std::hint::spin_loop();
                    }
                }
            });
        }
    })
    .unwrap();

    for c in v {
        assert_eq!(c.load(SeqCst), THREADS);
    }
}

#[test]
fn stampede() {
    const THREADS: usize = 8;
    #[cfg(miri)]
    const COUNT: usize = 500;
    #[cfg(not(miri))]
    const COUNT: usize = 50_000;

    let q = Injector::<Key, Arc<Task<usize>>>::new();

    for i in 0..COUNT {
        q.push(Arc::pin(Task::new(i + 1)));
    }
    let remaining = Arc::new(AtomicUsize::new(COUNT));

    scope(|scope| {
        for _ in 0..THREADS {
            let remaining = remaining.clone();
            let q = &q;

            scope.spawn(move |_| {
                let mut last = 0;
                while remaining.load(SeqCst) > 0 {
                    if let Success(x) = q.steal() {
                        assert!(last < x.value);
                        last = x.value;
                        remaining.fetch_sub(1, SeqCst);
                    }
                }
            });
        }

        let mut last = 0;
        while remaining.load(SeqCst) > 0 {
            if let Success(x) = q.steal() {
                assert!(last < x.value);
                last = x.value;
                remaining.fetch_sub(1, SeqCst);
            }
        }
    })
    .unwrap();
}

#[test]
fn stress() {
    const THREADS: usize = 8;
    #[cfg(miri)]
    const COUNT: usize = 500;
    #[cfg(not(miri))]
    const COUNT: usize = 50_000;

    let q = Injector::<Key, Arc<Task<usize>>>::new();
    let done = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));

    scope(|scope| {
        for _ in 0..THREADS {
            let done = done.clone();
            let hits = hits.clone();
            let q = &q;

            scope.spawn(move |_| {
                let w2 = Worker::new_fifo();

                while !done.load(SeqCst) {
                    if let Success(_) = q.steal() {
                        hits.fetch_add(1, SeqCst);
                    }

                    let _ = q.steal_batch(&w2);

                    if let Success(_) = q.steal_batch_and_pop(&w2) {
                        hits.fetch_add(1, SeqCst);
                    }

                    while w2.pop().is_some() {
                        hits.fetch_add(1, SeqCst);
                    }
                }
            });
        }

        let mut rng = rand::thread_rng();
        let mut expected = 0;
        while expected < COUNT {
            if rng.gen_range(0..3) == 0 {
                while let Success(_) = q.steal() {
                    hits.fetch_add(1, SeqCst);
                }
            } else {
                q.push(Arc::pin(Task::new(expected)));
                expected += 1;
            }
        }

        while hits.load(SeqCst) < COUNT {
            while let Success(_) = q.steal() {
                hits.fetch_add(1, SeqCst);
            }
        }
        done.store(true, SeqCst);
    })
    .unwrap();
}

#[cfg_attr(miri, ignore)] // Miri is too slow
#[test]
fn no_starvation() {
    const THREADS: usize = 8;
    const COUNT: usize = 50_000;

    let q = Injector::<Key, Arc<Task<usize>>>::new();
    let done = Arc::new(AtomicBool::new(false));
    let mut all_hits = Vec::new();

    scope(|scope| {
        for _ in 0..THREADS {
            let done = done.clone();
            let hits = Arc::new(AtomicUsize::new(0));
            all_hits.push(hits.clone());
            let q = &q;

            scope.spawn(move |_| {
                let w2 = Worker::new_fifo();

                while !done.load(SeqCst) {
                    if let Success(_) = q.steal() {
                        hits.fetch_add(1, SeqCst);
                    }

                    let _ = q.steal_batch(&w2);

                    if let Success(_) = q.steal_batch_and_pop(&w2) {
                        hits.fetch_add(1, SeqCst);
                    }

                    while w2.pop().is_some() {
                        hits.fetch_add(1, SeqCst);
                    }
                }
            });
        }

        let mut rng = rand::thread_rng();
        let mut my_hits = 0;
        loop {
            for i in 0..rng.gen_range(0..COUNT) {
                if rng.gen_range(0..3) == 0 && my_hits == 0 {
                    while let Success(_) = q.steal() {
                        my_hits += 1;
                    }
                } else {
                    q.push(Arc::pin(Task::new(i)));
                }
            }

            if my_hits > 0 && all_hits.iter().all(|h| h.load(SeqCst) > 0) {
                break;
            }
        }
        done.store(true, SeqCst);
    })
    .unwrap();
}

#[test]
fn destructors() {
    #[cfg(miri)]
    const THREADS: usize = 2;
    #[cfg(not(miri))]
    const THREADS: usize = 8;
    #[cfg(miri)]
    const COUNT: usize = 500;
    #[cfg(not(miri))]
    const COUNT: usize = 50_000;
    #[cfg(miri)]
    const STEPS: usize = 100;
    #[cfg(not(miri))]
    const STEPS: usize = 1000;

    struct Elem(usize, Arc<Mutex<Vec<usize>>>);

    impl Drop for Elem {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    type TaskTypes = crossbeam_intrusion::QueueTypes<Key, Arc<Task<Elem>>>;

    pin_project_lite::pin_project!(
        struct Task<V: ?Sized> {
            #[pin]
            intrusive: pin_queue::Intrusive<TaskTypes>,
            value: V,
        }
    );

    impl<V> Task<V> {
        pub fn new(value: V) -> Self {
            Self {
                intrusive: pin_queue::Intrusive::new(),
                value,
            }
        }
    }

    impl pin_queue::GetIntrusive<TaskTypes> for Key {
        fn get_intrusive(p: Pin<&Task<Elem>>) -> Pin<&pin_queue::Intrusive<TaskTypes>> {
            p.project_ref().intrusive
        }
    }

    let q = Injector::<Key, Arc<Task<Elem>>>::new();
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let remaining = Arc::new(AtomicUsize::new(COUNT));

    for i in 0..COUNT {
        q.push(Arc::pin(Task::new(Elem(i, dropped.clone()))));
    }

    scope(|scope| {
        for _ in 0..THREADS {
            let remaining = remaining.clone();
            let q = &q;

            scope.spawn(move |_| {
                let w2 = Worker::new_fifo();
                let mut cnt = 0;

                while cnt < STEPS {
                    if let Success(_) = q.steal() {
                        cnt += 1;
                        remaining.fetch_sub(1, SeqCst);
                    }

                    let _ = q.steal_batch(&w2);

                    if let Success(_) = q.steal_batch_and_pop(&w2) {
                        cnt += 1;
                        remaining.fetch_sub(1, SeqCst);
                    }

                    while w2.pop().is_some() {
                        cnt += 1;
                        remaining.fetch_sub(1, SeqCst);
                    }
                }
            });
        }

        for _ in 0..STEPS {
            if let Success(_) = q.steal() {
                remaining.fetch_sub(1, SeqCst);
            }
        }
    })
    .unwrap();

    let rem = remaining.load(SeqCst);
    assert!(rem > 0);

    {
        let mut v = dropped.lock().unwrap();
        assert_eq!(v.len(), COUNT - rem);
        v.clear();
    }

    drop(q);

    {
        let mut v = dropped.lock().unwrap();
        assert_eq!(v.len(), rem);
        v.sort_unstable();
        for pair in v.windows(2) {
            assert_eq!(pair[0] + 1, pair[1]);
        }
    }
}
