use tokio::sync::{Mutex, Semaphore, SemaphorePermit};

// Static collection of objects that can be temporarily allocated for mutually exclusive ownership.
pub struct Pool<T> {
    objs: Mutex<Vec<T>>,
    // If you can get this semaphore, items is guaranteed not to be empty.
    // This is a kinda weird workaround for the fact that there's no equivalent
    // to a Go channel in tokio and no condition variables.
    sem: Semaphore,
}

impl<T> Pool<T> {
    pub fn new<I>(objs: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        let vec: Vec<T> = objs.into_iter().collect();
        let len = vec.len();
        Self {
            objs: Mutex::new(vec),
            sem: Semaphore::new(len),
        }
    }
}

#[derive(Debug)]
pub struct PoolItem<'a, T: std::marker::Send> {
    obj: T,
    _permit: SemaphorePermit<'a>,
}

impl<T: std::marker::Send> AsRef<T> for PoolItem<'_, T> {
    fn as_ref(&self) -> &T {
        return &self.obj
    }
}

impl<T: std::marker::Send> Pool<T> {
    // Get an item from the pool, you must call put on it later. It sucks that it's this easy to
    // leak items. I thought we could just return them on Drop but it seems to be impossible to
    // actually do this in drop, since we need to await the lock. We could just put the cleanup into
    // a background thread but this then makes a huge mess since we would need to be able to mutate
    // the PoolItem in put, but then it still needs to be a valid object in Drop::drop (since you
    // only get a mutable reference to the object being dropped, not the object itself). This means
    // you'd end up needing some sort of mutation synchronization in PoolItem, even though it's
    // logically an immutable type. Yuck yuck yuck.
    pub async fn get(&self) -> PoolItem<T> {
        let permit = self
            .sem
            .acquire()
            .await
            .expect("Pool bug: semaphore closed");
        let mut objs = self.objs.lock().await;
        let obj = objs.pop().expect(
                "Pool empty when semaphore acquired . \
                This probably means a call to Pool::put was missed.",
        );
        PoolItem {
            obj,
            _permit: permit,
        }
    }

    // Return an item to the pool.
    pub async fn put(&self, item: PoolItem<'_, T>) {
        let PoolItem { obj, _permit } = item;
        let mut objs = self.objs.lock().await;
        objs.push(obj);
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use std::task::{Context, Poll};

    use futures::{pin_mut, task::noop_waker, Future};

    use super::*;

    // Assert that a future is blocked. Note that panicking directly in assertion helpers like this
    // is unhelpful because you lose line number info. It seems the proper solution for that is to
    // make them macros instead of functions. My solution is instead to just return errors and then
    // .expect() them, because I don't know how to make macros.
    fn check_pending<F>(fut: F) -> anyhow::Result<()>
    where
        F: Future,
        <F as futures::Future>::Output: std::fmt::Debug,
    {
        pin_mut!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Poll the future before it completes
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => Ok(()),
            Poll::Ready(res) => Err(anyhow!(
                "The future should be pending, but it produced {:?}",
                res
            )),
        }
    }

    #[test_log::test]
    fn test_empty_blocks() {
        let pool = Pool::<bool>::new([]);
        check_pending(pool.get()).expect("empty pool returned value");
    }

    #[test_log::test(tokio::test)]
    async fn test_get_some() {
        let pool = Pool::new([1, 2, 3]);
        // We don't actually functionally care about the order of the returned values, but
        //  - Stack order seems more cache-friendly
        //  - Asserting on the specific values is an easy way to check nothing insane is happening.
        let obj3 = pool.get().await;
        assert_eq!(obj3.obj, 3);
        let obj2 = pool.get().await;
        assert_eq!(obj2.obj, 2);
        let obj1 = pool.get().await;
        assert_eq!(obj1.obj, 1);
        let blocked_get = pool.get();
        check_pending(blocked_get).expect("empty pool returned value");
        pool.put(obj2).await;
        let obj2 = pool.get().await;
        assert_eq!(obj2.obj, 2);
    }
}
