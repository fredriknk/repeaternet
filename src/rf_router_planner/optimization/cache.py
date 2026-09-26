"""Bounded, single-flight storage for compact RF link metrics."""

from __future__ import annotations

from collections import OrderedDict
from collections.abc import Callable, Hashable
from dataclasses import dataclass, replace
from threading import Event, Lock

from rf_router_planner.models.link import LinkResult


@dataclass(slots=True)
class _Counters:
    hits: int = 0
    misses: int = 0
    evictions: int = 0


def _copy_metrics(link: LinkResult) -> LinkResult:
    """Copy only compact scalar/link metadata, never profile arrays."""
    return replace(
        link,
        forward=replace(link.forward),
        reverse=replace(link.reverse),
        dominant_obstacles=[],
        profile=None,
    )


class LinkMetricsCache:
    """An LRU cache that never retains sampled terrain profiles."""

    def __init__(self, max_entries: int = 50_000) -> None:
        if max_entries < 1:
            raise ValueError("max_entries must be positive")
        self.max_entries = max_entries
        self._entries: OrderedDict[tuple[Hashable, ...], LinkResult] = OrderedDict()
        self._inflight: dict[tuple[Hashable, ...], Event] = {}
        self._counters: dict[str, _Counters] = {}
        self._entry_counts: dict[str, int] = {}
        self._lock = Lock()

    def get_or_compute(
        self,
        namespace: str,
        key: tuple[Hashable, ...],
        compute: Callable[[], LinkResult],
    ) -> LinkResult:
        full_key: tuple[Hashable, ...] = (namespace, *key)
        while True:
            with self._lock:
                cached = self._entries.get(full_key)
                if cached is not None:
                    self._entries.move_to_end(full_key)
                    self._counter(namespace).hits += 1
                    return _copy_metrics(cached)
                event = self._inflight.get(full_key)
                if event is None:
                    event = Event()
                    self._inflight[full_key] = event
                    self._counter(namespace).misses += 1
                    owner = True
                else:
                    owner = False
            if not owner:
                event.wait()
                continue

            try:
                result = compute()
            except BaseException:
                with self._lock:
                    self._inflight.pop(full_key).set()
                raise

            compact = _copy_metrics(result)
            with self._lock:
                self._store_locked(namespace, full_key, compact)
                self._inflight.pop(full_key).set()
            return _copy_metrics(compact)

    def get(self, namespace: str, key: tuple[Hashable, ...]) -> LinkResult | None:
        full_key: tuple[Hashable, ...] = (namespace, *key)
        with self._lock:
            cached = self._entries.get(full_key)
            if cached is None:
                return None
            self._entries.move_to_end(full_key)
            self._counter(namespace).hits += 1
            return _copy_metrics(cached)

    def put(self, namespace: str, key: tuple[Hashable, ...], link: LinkResult) -> None:
        full_key: tuple[Hashable, ...] = (namespace, *key)
        with self._lock:
            if full_key in self._entries:
                self._entries.move_to_end(full_key)
                return
            self._counter(namespace).misses += 1
            self._store_locked(namespace, full_key, _copy_metrics(link))

    def stats(self, namespace: str) -> dict[str, int]:
        with self._lock:
            counters = self._counter(namespace)
            return {
                "hits": counters.hits,
                "misses": counters.misses,
                "evictions": counters.evictions,
                "entries": self._entry_counts.get(namespace, 0),
                "capacity": self.max_entries,
            }

    def clear(self, namespace: str) -> int:
        """Remove only one workspace's cached entries and reset its counters."""
        with self._lock:
            keys = [key for key in self._entries if key[0] == namespace]
            for key in keys:
                del self._entries[key]
            self._entry_counts[namespace] = 0
            self._counters[namespace] = _Counters()
            return len(keys)

    def _counter(self, namespace: str) -> _Counters:
        if namespace not in self._counters:
            self._counters[namespace] = _Counters()
        return self._counters[namespace]

    def _store_locked(
        self,
        namespace: str,
        key: tuple[Hashable, ...],
        link: LinkResult,
    ) -> None:
        self._entry_counts[namespace] = self._entry_counts.get(namespace, 0) + 1
        self._entries[key] = link
        self._entries.move_to_end(key)
        while len(self._entries) > self.max_entries:
            evicted_key, _ = self._entries.popitem(last=False)
            evicted_namespace = str(evicted_key[0])
            self._entry_counts[evicted_namespace] -= 1
            self._counter(evicted_namespace).evictions += 1
