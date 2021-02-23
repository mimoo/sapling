/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use futures::{
    future::{FutureExt, TryFutureExt},
    ready,
    stream::{self, FuturesUnordered, StreamExt},
    Stream,
};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    hash::Hash,
    iter::FromIterator,
    task::Poll,
};

/// `bounded_traversal_stream` traverses implicit asynchronous tree specified by `init`
/// and `unfold` arguments. All `unfold` operations are executed in parallel if they
/// do not depend on each other (not related by ancestor-descendant relation in implicit
/// tree) with amount of concurrency constrained by `scheduled_max`. Main difference
/// with `bounded_traversal` is that this one is not structure perserving, and returns
/// stream.
///
/// ## `init: InsInit`
/// Is the root(s) of the implicit tree to be traversed
///
/// ## `unfold: FnMut(In) -> impl Future<Output = Result<(Out, impl IntoIterator<Item = In>), UErr>>`
/// Asynchronous function which given input value produces list of its children and output
/// value.
///
/// ## return value `impl Stream<Item = Result<Out, UErr>>`
/// Stream of all `Out` values
///
pub fn bounded_traversal_stream<In, InsInit, Ins, Out, Unfold, UFut, UErr>(
    scheduled_max: usize,
    init: InsInit,
    mut unfold: Unfold,
) -> impl Stream<Item = Result<Out, UErr>>
where
    Unfold: FnMut(In) -> UFut,
    // TODO UFut could be IntoFuture once https://github.com/rust-lang/rust/pull/65244 is visible
    UFut: Future<Output = Result<(Out, Ins), UErr>>,
    InsInit: IntoIterator<Item = In>,
    Ins: IntoIterator<Item = In>,
{
    let mut unscheduled = VecDeque::from_iter(init);
    let mut scheduled = FuturesUnordered::new();
    stream::poll_fn(move |cx| {
        loop {
            if scheduled.is_empty() && unscheduled.is_empty() {
                return Poll::Ready(None);
            }

            for item in unscheduled
                .drain(..std::cmp::min(unscheduled.len(), scheduled_max - scheduled.len()))
            {
                scheduled.push(unfold(item))
            }

            if let Some((out, children)) = ready!(scheduled.poll_next_unpin(cx)).transpose()? {
                for child in children {
                    unscheduled.push_front(child);
                }
                return Poll::Ready(Some(Ok(out)));
            }
        }
    })
}

/// This function is similar to `bounded_traversal_stream` but:
///   - prevents items with duplicate keys executing concurrently
///   - allows an item to have no stream output by returning None
///   - optionally allows to restrict the number of keys executing concurrently by a ShardKey
pub fn limited_by_key_shardable<In, InsInit, Ins, Out, Unfold, UFut, UErr, Key, KeyFn, ShardKey>(
    scheduled_max: usize,
    init: InsInit,
    mut unfold: Unfold,
    key_fn: KeyFn,
) -> impl Stream<Item = Result<Out, UErr>>
where
    Unfold: FnMut(In) -> UFut,
    UFut: Future<Output = (Key, Option<ShardKey>, Result<Option<(Out, Ins)>, UErr>)>,
    InsInit: IntoIterator<Item = In>,
    Ins: IntoIterator<Item = In>,
    Key: Clone + Eq + Hash,
    KeyFn: Fn(&In) -> (&Key, Option<(ShardKey, usize)>),
    ShardKey: Clone + Eq + Hash,
{
    let mut unscheduled = VecDeque::from_iter(init);
    let mut scheduled = FuturesUnordered::new();
    let mut waiting_for_key: HashMap<Key, VecDeque<_>> = HashMap::new();
    let mut waiting_for_shard: HashMap<ShardKey, (usize, VecDeque<_>)> = HashMap::new();

    stream::poll_fn(move |cx| {
        loop {
            if scheduled.is_empty() && unscheduled.is_empty() {
                return Poll::Ready(None);
            }

            while scheduled.len() < scheduled_max && !unscheduled.is_empty() {
                for item in unscheduled
                    .drain(..std::cmp::min(unscheduled.len(), scheduled_max - scheduled.len()))
                {
                    let (key, shard_info) = key_fn(&item);
                    if let Some(inflight) = waiting_for_key.get_mut(key) {
                        // Exact duplicate, it needs to wait
                        inflight.push_back(item);
                        continue;
                    }

                    if let Some((shard_key, max_per_shard)) = shard_info {
                        let (inflight, queued) = waiting_for_shard.entry(shard_key).or_default();
                        if *inflight > max_per_shard {
                            // Shard is too busy, so queue more
                            queued.push_back(item);
                            continue;
                        } else {
                            *inflight += 1;
                        }
                    }

                    waiting_for_key.insert(key.clone(), VecDeque::new());
                    scheduled.push(unfold(item));
                }
            }

            if let Some((key, shard_key, unfolded)) = ready!(scheduled.poll_next_unpin(cx)) {
                if let Some((key, mut queue)) = waiting_for_key.remove_entry(&key) {
                    if let Some(item) = queue.pop_front() {
                        let unfolded = unfold(item);
                        scheduled.push(unfolded);
                    }
                    if !queue.is_empty() {
                        waiting_for_key.insert(key, queue);
                    }
                }

                if let Some(shard_key) = shard_key {
                    if let Some((inflight, queue)) = waiting_for_shard.get_mut(&shard_key) {
                        *inflight = inflight.saturating_sub(1);
                        if let Some(item) = queue.pop_front() {
                            // Don't directly schedule as could be a duplicate key
                            unscheduled.push_front(item);
                        }
                    }
                }

                if let Some((out, children)) = unfolded? {
                    // there is output on this unfold
                    for child in children {
                        unscheduled.push_front(child);
                    }
                    return Poll::Ready(Some(Ok(out)));
                }
            }
        }
    })
}

/// This function is similar to `bouned_traversal_stream`:
///   - but instead of iterator over children unfold returns a stream over children
///   - this stream must be `Unpin`
///   - if unscheduled queue is too large it will suspend iteration over children stream
pub fn bounded_traversal_stream2<In, Ins, Out, Unfold, UFut, UStream, UErr>(
    scheduled_max: usize,
    init: Ins,
    mut unfold: Unfold,
) -> impl Stream<Item = Result<Out, UErr>>
where
    Ins: IntoIterator<Item = In>,
    Unfold: FnMut(In) -> UFut,
    UFut: Future<Output = Result<(Out, UStream), UErr>>,
    UStream: Stream<Item = Result<In, UErr>> + Unpin,
{
    enum Op<U, C> {
        Unfold(U),
        Child(C),
    }

    let init = init
        .into_iter()
        .map(|child| unfold(child).map_ok(Op::Unfold).right_future());
    let mut unscheduled = VecDeque::from_iter(init);
    let mut scheduled = FuturesUnordered::new();
    stream::poll_fn(move |cx| {
        loop {
            if scheduled.is_empty() && unscheduled.is_empty() {
                return Poll::Ready(None);
            }

            while scheduled.len() < scheduled_max {
                match unscheduled.pop_front() {
                    Some(op) => scheduled.push(op),
                    None => break,
                }
            }

            if let Some(op) = ready!(scheduled.poll_next_unpin(cx)).transpose()? {
                match op {
                    Op::Unfold((out, children)) => {
                        let children = stream_into_try_future(children)
                            .map_ok(Op::Child)
                            .left_future();
                        unscheduled.push_back(children);
                        return Poll::Ready(Some(Ok(out)));
                    }
                    Op::Child((Some(child), children)) => {
                        unscheduled.push_back(unfold(child).map_ok(Op::Unfold).right_future());
                        let children = stream_into_try_future(children)
                            .map_ok(Op::Child)
                            .left_future();
                        // this will result in something like BFS (constraints to order of completion
                        // of scheduled tasks) traversal if unscheduled queue is small enough, otherwise
                        // it will suspend iteration over children and will put them in the unscheduled
                        // queue.
                        if unscheduled.len() > scheduled_max {
                            // we have too many unscheduled elements pause this children stream
                            unscheduled.push_back(children);
                        } else {
                            // continue polling for more children
                            scheduled.push(children);
                        }
                    }
                    _ => {}
                }
            }
        }
    })
}

fn stream_into_try_future<S, O, E>(stream: S) -> impl Future<Output = Result<(Option<O>, S), E>>
where
    S: Stream<Item = Result<O, E>> + Unpin,
{
    stream
        .into_future()
        .map(|(c, cs)| c.transpose().map(move |c| (c, cs)))
}
