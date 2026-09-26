from __future__ import annotations

from dataclasses import dataclass, field

from .link import LinkResult
from .site import Site


@dataclass(slots=True)
class NetworkSolution:
    """A complete mesh alternative for an exact router count.

    ``links`` contains every valid communicating link induced by the selected
    sites, rather than only the edges of one display route.  ``client_paths``
    stores one or more representative paths for each unordered client pair.
    """

    name: str
    sites: list[Site]
    links: list[LinkResult]
    client_ids: list[str]
    router_ids: list[str]
    client_paths: dict[tuple[str, str], list[list[str]]] = field(default_factory=dict)
    requested_path_count: int = 1
    achieved_path_count: int = 1
    diagnostics: list[str] = field(default_factory=list)

    @property
    def router_count(self) -> int:
        return len(self.router_ids)

    @property
    def resilient(self) -> bool:
        return self.achieved_path_count >= self.requested_path_count

    @property
    def minimum_margin_db(self) -> float:
        if not self.links:
            return float("-inf")
        return min(link.worst_margin_db for link in self.links)

    @property
    def primary_path_ids(self) -> list[str]:
        if not self.client_paths:
            return []
        paths = next(iter(self.client_paths.values()))
        return paths[0] if paths else []

    @property
    def primary_route(self) -> list[Site]:
        by_id = {site.id: site for site in self.sites}
        return [by_id[site_id] for site_id in self.primary_path_ids]
