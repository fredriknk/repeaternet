from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from rf_router_planner.models.settings import (
    AntennaSettings,
    CandidateSettings,
    DiffractionModel,
    LoRaSettings,
    OptimizationPriority,
    RFSettings,
    TerrainSettings,
    ValidationMode,
    dataclass_to_dict,
)
from rf_router_planner.models.site import HeightReference, Site, SiteKind, SiteOrigin

PROJECT_FORMAT_VERSION = 2


@dataclass(slots=True)
class Project:
    endpoint_a: Site | None = None
    endpoint_b: Site | None = None
    selected_routers: list[Site] = field(default_factory=list)
    rf_settings: RFSettings = field(default_factory=RFSettings.eu868_meshcore)
    terrain_settings: TerrainSettings = field(default_factory=TerrainSettings)
    candidate_settings: CandidateSettings = field(default_factory=CandidateSettings)
    exclusion_areas: list[list[tuple[float, float]]] = field(default_factory=list)
    additional_clients: list[Site] = field(default_factory=list)
    manual_routers: list[Site] = field(default_factory=list)
    known_routers: list[Site] = field(default_factory=list)


def _site_from_dict(data: dict[str, Any] | None) -> Site | None:
    if data is None:
        return None
    copy = dict(data)
    copy["kind"] = SiteKind(copy.get("kind", SiteKind.CANDIDATE.value))
    copy["height_reference"] = HeightReference(
        copy.get("height_reference", HeightReference.GROUND_DTM.value)
    )
    copy["origin"] = SiteOrigin(copy.get("origin", SiteOrigin.OPTIMIZED.value))
    return Site(**copy)


def _sites_from_list(values: list[dict[str, Any]]) -> list[Site]:
    sites: list[Site] = []
    for value in values:
        site = _site_from_dict(value)
        if site is not None:
            sites.append(site)
    return sites


def save_project(project: Project, path: str | Path) -> None:
    document = {
        "format": "rf-router-planner",
        "version": PROJECT_FORMAT_VERSION,
        "endpoint_a": dataclass_to_dict(project.endpoint_a),
        "endpoint_b": dataclass_to_dict(project.endpoint_b),
        "selected_routers": dataclass_to_dict(project.selected_routers),
        "rf_settings": dataclass_to_dict(project.rf_settings),
        "terrain_settings": dataclass_to_dict(project.terrain_settings),
        "candidate_settings": dataclass_to_dict(project.candidate_settings),
        "exclusion_areas": project.exclusion_areas,
        "additional_clients": dataclass_to_dict(project.additional_clients),
        "manual_routers": dataclass_to_dict(project.manual_routers),
        "known_routers": dataclass_to_dict(project.known_routers),
    }
    Path(path).write_text(json.dumps(document, indent=2), encoding="utf-8")


def load_project(path: str | Path) -> Project:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if data.get("format") != "rf-router-planner" or data.get("version") not in {1, 2}:
        raise ValueError("Unsupported project file")
    rf_data = data.get("rf_settings", {})
    rf_data["diffraction_model"] = DiffractionModel(
        rf_data.get("diffraction_model", DiffractionModel.DEYGOUT.value)
    )
    endpoint_a_antenna = AntennaSettings(**rf_data.pop("endpoint_a", {}))
    endpoint_b_antenna = AntennaSettings(**rf_data.pop("endpoint_b", {}))
    router_antenna = AntennaSettings(**rf_data.pop("router", {}))
    lora = LoRaSettings(**rf_data.pop("lora", {}))
    rf_data["validation_mode"] = ValidationMode(
        rf_data.get("validation_mode", ValidationMode.PROPAGATION.value)
    )
    rf = RFSettings(
        **rf_data,
        endpoint_a=endpoint_a_antenna,
        endpoint_b=endpoint_b_antenna,
        router=router_antenna,
        lora=lora,
    )
    candidate_data = data.get("candidate_settings", {})
    candidate_data["priority"] = OptimizationPriority(
        candidate_data.get("priority", OptimizationPriority.MINIMUM_ROUTERS.value)
    )
    return Project(
        _site_from_dict(data.get("endpoint_a")),
        _site_from_dict(data.get("endpoint_b")),
        [_site_from_dict(item) for item in data.get("selected_routers", []) if item],  # type: ignore[misc]
        rf,
        TerrainSettings(**data.get("terrain_settings", {})),
        CandidateSettings(**candidate_data),
        data.get("exclusion_areas", []),
        _sites_from_list(data.get("additional_clients", [])),
        _sites_from_list(data.get("manual_routers", [])),
        _sites_from_list(data.get("known_routers", [])),
    )
