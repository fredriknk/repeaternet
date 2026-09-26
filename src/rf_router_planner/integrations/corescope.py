from __future__ import annotations

import time
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any


@dataclass(frozen=True, slots=True)
class CoreScopeRepeater:
    public_key: str
    name: str
    latitude: float
    longitude: float
    last_heard: datetime | None = None
    relay_active: bool | None = None
    relay_count_24h: int = 0

    @property
    def id(self) -> str:
        return f"K-{self.public_key[:10]}"

    @property
    def freshness(self) -> str:
        if not self.last_heard:
            return "last heard unknown"
        age = datetime.now(UTC) - self.last_heard
        if age.days:
            return f"heard {age.days} day(s) ago"
        hours = max(0, int(age.total_seconds() // 3600))
        return f"heard {hours} hour(s) ago"


class CoreScopeClient:
    """Small, permissive read-only client for a CoreScope node feed."""

    def __init__(
        self,
        base_url: str = "https://corescope.eth0.no",
        *,
        timeout_seconds: float = 20,
        page_size: int = 500,
        session: Any | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout_seconds = timeout_seconds
        self.page_size = page_size
        self._session = session

    def fetch_repeaters(self, maximum_pages: int = 20) -> list[CoreScopeRepeater]:
        if self._session is None:
            try:
                import requests
            except ImportError as exc:  # pragma: no cover - application dependency
                raise RuntimeError("Requests is required for CoreScope import") from exc
            self._session = requests.Session()
        repeaters: dict[str, CoreScopeRepeater] = {}
        for page in range(maximum_pages):
            offset = page * self.page_size
            response = self._get_with_retry(
                "/api/nodes",
                {"role": "repeater", "limit": self.page_size, "offset": offset},
            )
            payload = response.json()
            records = payload.get("nodes", []) if isinstance(payload, dict) else []
            if not isinstance(records, list):
                raise ValueError("CoreScope returned an invalid node list")
            added = 0
            for record in records:
                parsed = self._parse_repeater(record)
                if parsed and parsed.public_key not in repeaters:
                    repeaters[parsed.public_key] = parsed
                    added += 1
            total = payload.get("total") if isinstance(payload, dict) else None
            if len(records) < self.page_size or not added:
                break
            if isinstance(total, int) and len(repeaters) >= total:
                break
        return sorted(repeaters.values(), key=lambda item: item.name.casefold())

    def _get_with_retry(self, path: str, params: dict[str, object]) -> Any:
        session = self._session
        if session is None:  # pragma: no cover - guarded by fetch_repeaters
            raise RuntimeError("CoreScope HTTP session is not initialized")
        headers = {"User-Agent": "RF-Router-Planner/0.1 (interactive read-only import)"}
        for attempt in range(3):
            response = session.get(
                self.base_url + path,
                params=params,
                headers=headers,
                timeout=self.timeout_seconds,
            )
            if response.status_code not in {429, 503}:
                response.raise_for_status()
                return response
            if attempt < 2:
                retry_after = response.headers.get("Retry-After", "1")
                try:
                    delay = min(5.0, max(0.0, float(retry_after)))
                except ValueError:
                    delay = 1.0
                time.sleep(delay)
        response.raise_for_status()
        return response  # pragma: no cover

    @staticmethod
    def _parse_repeater(value: object) -> CoreScopeRepeater | None:
        if not isinstance(value, dict) or value.get("role") != "repeater":
            return None
        public_key = str(value.get("public_key") or "").strip()
        try:
            latitude = float(value["lat"])
            longitude = float(value["lon"])
        except (KeyError, TypeError, ValueError):
            return None
        if not public_key or (latitude == 0 and longitude == 0):
            return None
        if not 54 <= latitude <= 72 or not 4 <= longitude <= 32:
            return None
        heard = CoreScopeClient._parse_time(value.get("last_heard") or value.get("last_seen"))
        return CoreScopeRepeater(
            public_key,
            str(value.get("name") or public_key[:10]),
            latitude,
            longitude,
            heard,
            value.get("relay_active") if isinstance(value.get("relay_active"), bool) else None,
            int(value.get("relay_count_24h") or 0),
        )

    @staticmethod
    def _parse_time(value: object) -> datetime | None:
        if not isinstance(value, str) or not value:
            return None
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None
