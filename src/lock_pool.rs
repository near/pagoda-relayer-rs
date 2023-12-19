use std::{ops::Deref, sync::Arc};

#[derive(Debug, Clone)]
pub struct LockPool<T> {
    pub(self) send: flume::Sender<Arc<T>>,
    pub(self) recv: flume::Receiver<Arc<T>>,
}

pub struct LockPoolGuard<T> {
    item: Arc<T>,
    return_to: flume::Sender<Arc<T>>,
}

impl<T> Deref for LockPoolGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.item.deref()
    }
}

impl<T> Drop for LockPoolGuard<T> {
    fn drop(&mut self) {
        // only fails if the pool has been dropped
        let _ = self.return_to.send(Arc::clone(&self.item));
    }
}

impl<T> LockPool<T> {
    pub fn new(items: impl IntoIterator<Item = T>) -> Self {
        let (send, recv) = flume::unbounded();

        for i in items {
            send.send(Arc::new(i)).unwrap();
        }

        Self { send, recv }
    }

    pub async fn request(&self) -> LockPoolGuard<T> {
        let item = self.recv.recv_async().await.unwrap();
        LockPoolGuard {
            item,
            return_to: self.send.clone(),
        }
    }

    pub fn request_blocking(&self) -> LockPoolGuard<T> {
        let item = self.recv.recv().unwrap();
        LockPoolGuard {
            item,
            return_to: self.send.clone(),
        }
    }
}

#[tokio::test]
async fn test() {
    let pool = LockPool::new(vec![1, 2, 3]);

    let mut handles = vec![];

    for i in 0..10 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            println!("{i}: requesting");
            let share = pool.request().await;
            println!("{i}: obtained {}", *share);
            tokio::time::sleep(std::time::Duration::from_millis(i * 100)).await;
            println!("{i}: dropping {}", *share);
            drop(share);
            println!("{i}: dropped");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}
