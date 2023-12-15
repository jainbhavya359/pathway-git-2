pub mod gradual_broadcast;
pub mod output;
pub mod prev_next;
pub mod stateful_reduce;
pub mod time_column;
mod utils;

use std::any::type_name;
use std::panic::Location;

use differential_dataflow::difference::Semigroup;
use differential_dataflow::operators::arrange::{Arranged, TraceAgent};
use differential_dataflow::trace::{Batch, Trace, TraceReader};
use differential_dataflow::{AsCollection, Collection, Data, ExchangeData};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use futures::{future, Future};
use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::operators::Exchange as _;
use timely::dataflow::operators::Operator;

use crate::engine::BatchWrapper;

use super::maybe_total::{MaybeTotalScope, MaybeTotalSwitch};
use super::shard::Shard;
use super::ArrangedBySelf;

pub trait ArrangeWithTypes<S, K, V, R>
where
    S: MaybeTotalScope,
    K: ExchangeData,
    V: ExchangeData,
    R: Semigroup + ExchangeData,
{
    #[track_caller]
    fn arrange<Tr>(&self) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch,
    {
        self.arrange_named("Arrange")
    }

    fn arrange_named<Tr>(&self, name: &str) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch;
}

pub trait ArrangeWithTypesSharded<S, K, V, R>
where
    S: MaybeTotalScope,
    K: ExchangeData,
    V: ExchangeData,
    R: Semigroup + ExchangeData,
{
    #[track_caller]
    fn arrange_sharded<Tr>(
        &self,
        sharding: impl FnMut(&K) -> u64 + 'static,
    ) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch,
    {
        self.arrange_sharded_named("Arrange", sharding)
    }

    fn arrange_sharded_named<Tr>(
        &self,
        name: &str,
        sharding: impl FnMut(&K) -> u64 + 'static,
    ) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch;
}

impl<T, S, K, V, R> ArrangeWithTypes<S, K, V, R> for T
where
    T: differential_dataflow::operators::arrange::arrangement::Arrange<S, K, V, R>,
    S: MaybeTotalScope,
    K: ExchangeData + Shard,
    V: ExchangeData,
    R: Semigroup + ExchangeData,
{
    #[track_caller]
    fn arrange_named<Tr>(&self, name: &str) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch,
    {
        self.arrange_sharded_named(name, Shard::shard)
    }
}

impl<T, S, K, V, R> ArrangeWithTypesSharded<S, K, V, R> for T
where
    T: differential_dataflow::operators::arrange::arrangement::Arrange<S, K, V, R>,
    S: MaybeTotalScope,
    K: ExchangeData,
    V: ExchangeData,
    R: Semigroup + ExchangeData,
{
    #[track_caller]
    fn arrange_sharded_named<Tr>(
        &self,
        name: &str,
        mut sharding: impl FnMut(&K) -> u64 + 'static,
    ) -> Arranged<S, TraceAgent<Tr>>
    where
        Tr: Trace + TraceReader<Key = K, Val = V, Time = S::Timestamp, R = R> + 'static,
        Tr::Batch: Batch,
    {
        let caller = Location::caller();
        let name = format!(
            "{name} [{key}, {value}] at {caller}",
            key = type_name::<K>(),
            value = type_name::<V>()
        );
        let exchange =
            Exchange::new(move |((key, _value), _time, _diff): &((K, V), _, _)| sharding(key));
        #[allow(clippy::disallowed_methods)]
        differential_dataflow::operators::arrange::arrangement::Arrange::arrange_core(
            self, exchange, &name,
        )
    }
}

pub trait MaybeTotal<S, K, R>
where
    S: MaybeTotalScope,
    K: Data,
    R: Semigroup,
{
    fn count(&self) -> Collection<S, (K, R), isize>;

    fn distinct(&self) -> Collection<S, K, isize>;
}

impl<S, T> MaybeTotal<S, T::Key, T::R> for Arranged<S, T>
where
    S: MaybeTotalScope,
    T: TraceReader<Val = (), Time = S::Timestamp> + Clone + 'static,
    T::Key: Data,
    T::R: Semigroup,
{
    fn count(&self) -> Collection<S, (T::Key, T::R), isize> {
        S::IsTotal::count(self)
    }

    fn distinct(&self) -> Collection<S, T::Key, isize> {
        S::IsTotal::distinct(self)
    }
}

impl<S, K, R> MaybeTotal<S, K, R> for Collection<S, K, R>
where
    S: MaybeTotalScope,
    K: ExchangeData + Shard,
    R: Semigroup + ExchangeData,
{
    fn count(&self) -> Collection<S, (K, R), isize> {
        let arranged: ArrangedBySelf<S, K, R> = self.arrange_named("Arrange: CountMaybeTotal");
        arranged.count()
    }

    fn distinct(&self) -> Collection<S, K, isize> {
        let arranged: ArrangedBySelf<S, K, R> = self.arrange_named("Arrange: DistinctMaybeTotal");
        arranged.distinct()
    }
}

pub trait MapWrapped<S, D, R>
where
    S: MaybeTotalScope,
    R: Semigroup,
{
    #[track_caller]
    fn map_wrapped<D2: Data>(
        &self,
        wrapper: BatchWrapper,
        logic: impl FnMut(D) -> D2 + 'static,
    ) -> Collection<S, D2, R> {
        let name = format!("MapWrapped({wrapper:?})");
        self.map_wrapped_named(&name, wrapper, logic)
    }

    #[track_caller]
    fn map_ex<D2: Data>(&self, logic: impl FnMut(D) -> D2 + 'static) -> Collection<S, D2, R> {
        self.map_wrapped_named("MapEx", BatchWrapper::None, logic)
    }

    #[track_caller]
    fn map_named<D2: Data>(
        &self,
        name: &str,
        logic: impl FnMut(D) -> D2 + 'static,
    ) -> Collection<S, D2, R> {
        self.map_wrapped_named(name, BatchWrapper::None, logic)
    }

    fn map_wrapped_named<D2: Data>(
        &self,
        name: &str,
        wrapper: BatchWrapper,
        logic: impl FnMut(D) -> D2 + 'static,
    ) -> Collection<S, D2, R>;

    fn map_named_async<F: Future>(
        &self,
        name: &str,
        logic: impl Fn(D) -> F + 'static,
    ) -> Collection<S, F::Output, R>
    where
        F::Output: Data;

    fn map_async<F: Future>(&self, logic: impl Fn(D) -> F + 'static) -> Collection<S, F::Output, R>
    where
        F::Output: Data,
    {
        self.map_named_async("MapAsync", logic)
    }
}

impl<S, D, R> MapWrapped<S, D, R> for Collection<S, D, R>
where
    S: MaybeTotalScope,
    D: Data,
    R: Semigroup,
{
    #[track_caller]
    fn map_wrapped_named<D2: Data>(
        &self,
        name: &str,
        wrapper: BatchWrapper,
        mut logic: impl FnMut(D) -> D2 + 'static,
    ) -> Collection<S, D2, R> {
        let caller = Location::caller();
        let name = format!("{name} at {caller}");
        let mut vector = Vec::new();
        self.inner
            .unary(Pipeline, &name, move |_, _| {
                move |input, output| {
                    wrapper.run(|| {
                        while let Some((time, data)) = input.next() {
                            data.swap(&mut vector);
                            output.session(&time).give_iterator(
                                vector
                                    .drain(..)
                                    .map(|(data, time, diff)| (logic(data), time, diff)),
                            );
                        }
                    });
                }
            })
            .as_collection()
    }

    #[track_caller]
    fn map_named_async<F: Future>(
        &self,
        name: &str,
        logic: impl Fn(D) -> F + 'static,
    ) -> Collection<S, F::Output, R>
    where
        F::Output: Data,
    {
        let caller = Location::caller();
        let name = format!("{name} at {caller}");
        let mut vector = Vec::new();
        let mut result = Vec::new();
        self.inner
            .unary(Pipeline, &name, move |_, _| {
                move |input, output| {
                    while let Some((time, data)) = input.next() {
                        data.swap(&mut vector);

                        let futures: FuturesUnordered<_> = vector
                            .drain(..)
                            .map(|(data, time, diff)| async { (logic(data).await, time, diff) })
                            .collect();

                        assert!(result.is_empty());
                        result.reserve(futures.len());

                        futures::executor::block_on(futures.for_each(|item| {
                            result.push(item);
                            future::ready(())
                        }));

                        output.session(&time).give_vec(&mut result);
                    }
                }
            })
            .as_collection()
    }
}

pub trait Reshard<S, D, R>
where
    S: MaybeTotalScope,
    D: Data,
    R: Semigroup,
{
    fn reshard(&self) -> Collection<S, D, R>;
}

impl<S, D, R> Reshard<S, D, R> for Collection<S, D, R>
where
    S: MaybeTotalScope,
    D: ExchangeData + Shard,
    R: ExchangeData + Semigroup,
{
    fn reshard(&self) -> Collection<S, D, R> {
        self.inner
            .exchange(|(data, _time, _diff)| data.shard())
            .as_collection()
    }
}