use super::{AsyncReadableFileStream, FileType};
use std::{ops::Deref, path::PathBuf, sync::Arc};

type IsIgnoredFnInner = dyn Fn(FileType, PathBuf) -> Option<PathBuf> + Send + Sync + 'static;
type AsyncIsIgnoredFnInner = dyn Fn(FileType, PathBuf) -> futures::future::BoxFuture<'static, Option<PathBuf>>
    + Send
    + Sync
    + 'static;

/// One deny-list filter carrying both ways to run it.
///
/// A single filter is reachable from both halves of the filesystem traits, so
/// splitting it into a sync and an async type would mean threading two values
/// everywhere and letting a caller configure one but not the other. Instead the
/// caller picks a strategy by context: sync bodies deref to the blocking
/// closure, async ones use [`IsIgnoredFn::call_async`].
///
/// Filters that do no I/O leave the async half unset and run inline, so only a
/// filter that genuinely awaits pays for a boxed future.
#[derive(Clone)]
pub struct IsIgnoredFn {
    sync: Arc<IsIgnoredFnInner>,
    r#async: Option<Arc<AsyncIsIgnoredFnInner>>,
}

impl IsIgnoredFn {
    /// Pairs a blocking body with the async one it mirrors.
    ///
    /// Both must accept and reject exactly the same paths; only how they get
    /// there may differ. For anything that does no I/O, prefer `from` — the
    /// single body is then reused for both.
    pub fn new<S, A, Fut>(sync: S, r#async: A) -> Self
    where
        S: Fn(FileType, PathBuf) -> Option<PathBuf> + Send + Sync + 'static,
        A: Fn(FileType, PathBuf) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<PathBuf>> + Send + 'static,
    {
        Self {
            sync: Arc::new(sync),
            r#async: Some(Arc::new(move |file_type, path| {
                Box::pin(r#async(file_type, path))
            })),
        }
    }

    pub async fn call_async(&self, file_type: FileType, path: PathBuf) -> Option<PathBuf> {
        match &self.r#async {
            Some(r#async) => r#async(file_type, path).await,
            None => (self.sync)(file_type, path),
        }
    }

    pub fn merge(self, other: IsIgnoredFn) -> IsIgnoredFn {
        let (first, second) = (Arc::clone(&self.sync), Arc::clone(&other.sync));
        let sync: Arc<IsIgnoredFnInner> =
            Arc::new(move |file_type, path| second(file_type, first(file_type, path)?));

        if self.r#async.is_none() && other.r#async.is_none() {
            return Self::from_sync(sync);
        }

        Self {
            sync,
            r#async: Some(Arc::new(move |file_type, path| {
                let (first, second) = (self.clone(), other.clone());

                Box::pin(async move {
                    let path = first.call_async(file_type, path).await?;

                    second.call_async(file_type, path).await
                })
            })),
        }
    }

    #[inline]
    fn from_sync(sync: Arc<IsIgnoredFnInner>) -> Self {
        Self {
            sync,
            r#async: None,
        }
    }
}

impl Default for IsIgnoredFn {
    fn default() -> Self {
        Self::from_sync(Arc::new(|_, path| Some(path)))
    }
}

impl Deref for IsIgnoredFn {
    type Target = IsIgnoredFnInner;

    fn deref(&self) -> &Self::Target {
        &*self.sync
    }
}

impl From<ignore::gitignore::Gitignore> for IsIgnoredFn {
    fn from(gi: ignore::gitignore::Gitignore) -> Self {
        Self::from_sync(Arc::new(move |file_type, path| {
            if gi.matched(&path, file_type.is_dir()).is_ignore() {
                None
            } else {
                Some(path)
            }
        }))
    }
}

impl From<Vec<ignore::gitignore::Gitignore>> for IsIgnoredFn {
    fn from(gis: Vec<ignore::gitignore::Gitignore>) -> Self {
        Self::from_sync(Arc::new(move |file_type, path| {
            for gi in &gis {
                if gi.matched(&path, file_type.is_dir()).is_ignore() {
                    return None;
                }
            }
            Some(path)
        }))
    }
}

impl<T: Fn(FileType, PathBuf) -> Option<PathBuf> + Send + Sync + 'static> From<T> for IsIgnoredFn {
    fn from(f: T) -> Self {
        Self::from_sync(Arc::new(f))
    }
}

type DirectoryWalkFnInner =
    dyn Fn(FileType, PathBuf) -> Result<(), anyhow::Error> + Send + Sync + 'static;

#[derive(Clone)]
pub struct DirectoryWalkFn(Arc<DirectoryWalkFnInner>);

impl<T: Fn(FileType, PathBuf) -> Result<(), anyhow::Error> + Send + Sync + 'static> From<T>
    for DirectoryWalkFn
{
    fn from(f: T) -> Self {
        Self(Arc::new(f))
    }
}

impl Deref for DirectoryWalkFn {
    type Target = DirectoryWalkFnInner;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

type AsyncDirectoryWalkFnInner = dyn Fn(FileType, PathBuf) -> futures::future::BoxFuture<'static, Result<(), anyhow::Error>>
    + Send
    + Sync
    + 'static;

#[derive(Clone)]
pub struct AsyncDirectoryWalkFn(Arc<AsyncDirectoryWalkFnInner>);

impl<
    T: Fn(FileType, PathBuf) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), anyhow::Error>> + Send + 'static,
> From<T> for AsyncDirectoryWalkFn
{
    fn from(f: T) -> Self {
        Self(Arc::new(move |file_type, path| {
            let fut = f(file_type, path);
            Box::pin(fut)
        }))
    }
}

impl Deref for AsyncDirectoryWalkFn {
    type Target = AsyncDirectoryWalkFnInner;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

type DirectoryStreamWalkFnInner = dyn Fn(
        FileType,
        PathBuf,
        AsyncReadableFileStream,
    ) -> futures::future::BoxFuture<'static, Result<(), anyhow::Error>>
    + Send
    + Sync
    + 'static;

#[derive(Clone)]
pub struct AsyncDirectoryStreamWalkFn(Arc<DirectoryStreamWalkFnInner>);

impl<
    T: Fn(FileType, PathBuf, AsyncReadableFileStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), anyhow::Error>> + Send + 'static,
> From<T> for AsyncDirectoryStreamWalkFn
{
    fn from(f: T) -> Self {
        Self(Arc::new(move |file_type, path, stream| {
            let fut = f(file_type, path, stream);
            Box::pin(fut)
        }))
    }
}

impl Deref for AsyncDirectoryStreamWalkFn {
    type Target = DirectoryStreamWalkFnInner;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // IsIgnoredFn sync/async parity

    fn deny(name: &'static str) -> IsIgnoredFn {
        IsIgnoredFn::from(move |_: FileType, path: PathBuf| {
            if path.ends_with(name) {
                None
            } else {
                Some(path)
            }
        })
    }

    #[test]
    fn call_async_falls_back_to_the_sync_body_when_unset() {
        tokio_test::block_on(async {
            let filter = deny("secret.yml");

            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("a/secret.yml"))
                    .await,
                None
            );
            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("a/ok.yml"))
                    .await,
                Some(PathBuf::from("a/ok.yml"))
            );
        });
    }

    #[test]
    fn new_runs_each_body_on_its_matching_call() {
        tokio_test::block_on(async {
            let filter = IsIgnoredFn::new(
                |_, path: PathBuf| Some(path.join("sync")),
                |_, path: PathBuf| async move { Some(path.join("async")) },
            );

            assert_eq!(
                (filter)(FileType::File, PathBuf::from("root")),
                Some(PathBuf::from("root/sync"))
            );
            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("root"))
                    .await,
                Some(PathBuf::from("root/async"))
            );
        });
    }

    #[test]
    fn merge_rejects_when_either_side_rejects() {
        tokio_test::block_on(async {
            let filter = deny("a.txt").merge(deny("b.txt"));

            for denied in ["a.txt", "b.txt"] {
                assert_eq!((filter)(FileType::File, PathBuf::from(denied)), None);
                assert_eq!(
                    filter
                        .call_async(FileType::File, PathBuf::from(denied))
                        .await,
                    None
                );
            }

            assert_eq!(
                (filter)(FileType::File, PathBuf::from("c.txt")),
                Some(PathBuf::from("c.txt"))
            );
            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("c.txt"))
                    .await,
                Some(PathBuf::from("c.txt"))
            );
        });
    }

    #[test]
    fn merge_threads_the_rewritten_path_through_in_order() {
        tokio_test::block_on(async {
            let first = IsIgnoredFn::from(|_: FileType, p: PathBuf| Some(p.join("first")));
            let second = IsIgnoredFn::from(|_: FileType, p: PathBuf| Some(p.join("second")));
            let filter = first.merge(second);

            assert_eq!(
                (filter)(FileType::File, PathBuf::from("root")),
                Some(PathBuf::from("root/first/second"))
            );
            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("root"))
                    .await,
                Some(PathBuf::from("root/first/second"))
            );
        });
    }

    #[test]
    fn merge_keeps_the_async_body_of_a_paired_side() {
        tokio_test::block_on(async {
            let paired = IsIgnoredFn::new(
                |_, path: PathBuf| Some(path.join("sync")),
                |_, path: PathBuf| async move { Some(path.join("async")) },
            );
            let filter = paired.merge(deny("nothing"));

            assert_eq!(
                filter
                    .call_async(FileType::File, PathBuf::from("root"))
                    .await,
                Some(PathBuf::from("root/async"))
            );
        });
    }

    #[test]
    fn default_keeps_every_path() {
        tokio_test::block_on(async {
            let filter = IsIgnoredFn::default();

            assert_eq!(
                filter
                    .call_async(FileType::Dir, PathBuf::from("anything"))
                    .await,
                Some(PathBuf::from("anything"))
            );
        });
    }
}
