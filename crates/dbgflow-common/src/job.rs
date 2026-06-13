use crate::{DbgFlowError, Result};
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct SingleActiveJob<I: Copy + Eq> {
    active: Arc<Mutex<Option<I>>>,
}

impl<I: Copy + Eq> Clone for SingleActiveJob<I> {
    fn clone(&self) -> Self {
        Self {
            active: self.active.clone(),
        }
    }
}

impl<I: Copy + Eq> Default for SingleActiveJob<I> {
    fn default() -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
        }
    }
}

impl<I> SingleActiveJob<I>
where
    I: Copy + Eq + fmt::Display,
{
    pub fn start(
        &self,
        id: I,
        busy_message: impl FnOnce(I) -> String,
    ) -> Result<SingleActiveJobGuard<I>> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| DbgFlowError::Backend("active job lock poisoned".to_string()))?;
        if let Some(active_id) = *active {
            return Err(DbgFlowError::Backend(busy_message(active_id)));
        }
        *active = Some(id);
        Ok(SingleActiveJobGuard {
            active: self.active.clone(),
            id,
        })
    }
}

#[derive(Debug)]
pub struct SingleActiveJobGuard<I: Copy + Eq> {
    active: Arc<Mutex<Option<I>>>,
    id: I,
}

impl<I: Copy + Eq> Drop for SingleActiveJobGuard<I> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            if *active == Some(self.id) {
                *active = None;
            }
        }
    }
}
