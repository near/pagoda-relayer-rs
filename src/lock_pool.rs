use std::{ops::Deref, sync::Arc};

use near_primitives::types::AccountId;

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

// TODO remove after fix
// Attempted fix to the following error:
/*
error[E0277]: the trait bound `LockPool<InMemorySigner>: Signer` is not satisfied
   --> src/main.rs:864:26
    |
864 |                 .send_tx(&*SIGNER, &receiver_id, actions)
    |                          ^^^^^^^^ the trait `Signer` is not implemented for `LockPool<InMemorySigner>`
    |
    = help: the following other types implement trait `Signer`:
              EmptySigner
              KeyRotatingSigner
              InMemorySigner
    = note: required for `LockPool<InMemorySigner>` to implement `SignerExt`
    = note: required for the cast from `&LockPool<InMemorySigner>` to `&dyn SignerExt`
 */
// trait ExposeAccountId {
//     fn account_id(&self) -> AccountId;
// }
// // Implement `ExposeAccountId` for `LockPool<T>` where `T: ExposeAccountId`
// impl<T: ExposeAccountId> ExposeAccountId for LockPool<T> {
//     fn account_id(&self) -> AccountId {
//         // Here you need to decide how to handle the fact that `LockPool` may contain multiple instances of `T`.
//         // For example, you might always fetch an instance from the pool and use it to get the `AccountId`,
//         // or you might have a default or static `AccountId` that's used for the pool as a whole.
//         // This is a simplified example where we just fetch an instance and use it.
//         let guard = self.request_blocking(); // or use `request` in async contexts
//         guard.account_id()
//     }
// }

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
