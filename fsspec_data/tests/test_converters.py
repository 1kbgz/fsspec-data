from types import SimpleNamespace

import fsspec
import pytest

import fsspec_data.converters as converters_module
import fsspec_data.xarray_zarr as xarray_zarr_module
from fsspec_data import DEFAULT_CONVERTERS, Converter, ConverterRegistry


def test_converter_registry_preserves_scoped_options():
    received = []

    def convert(source, target, **options):
        received.append((source, target, options))
        return "converted"

    registry = ConverterRegistry(discover_entry_points=False)
    registry.register(Converter(" Source ", "TARGET", convert))

    result = registry.convert(
        "source",
        "target",
        "input",
        "output",
        source_options={"source-token": "a"},
        target_options={"target-token": "b"},
        conversion_options={"mode": "safe"},
    )

    assert result == "converted"
    assert received == [
        (
            "input",
            "output",
            {
                "source_options": {"source-token": "a"},
                "target_options": {"target-token": "b"},
                "conversion_options": {"mode": "safe"},
            },
        )
    ]


def test_converter_registry_rejects_duplicate_route():
    registry = ConverterRegistry(discover_entry_points=False)
    converter = Converter("source", "target", lambda *args, **kwargs: None)
    registry.register(converter)

    with pytest.raises(ValueError, match="already registered"):
        registry.register(converter)


def test_converter_registry_loads_entry_points_lazily(monkeypatch):
    loaded = []
    converter = Converter("custom-source", "custom-target", lambda *args, **kwargs: "plugin")

    class EntryPoint:
        name = "custom"
        value = "example.converter:CONVERTER"

        def load(self):
            loaded.append(self.name)
            return converter

    monkeypatch.setattr(
        converters_module.importlib.metadata,
        "entry_points",
        lambda **selection: (EntryPoint(),),
    )
    registry = ConverterRegistry()

    assert loaded == []
    assert registry.convert("custom-source", "custom-target", "value") == "plugin"
    assert loaded == ["custom"]
    registry.get("custom-source", "custom-target")
    assert loaded == ["custom"]


def test_converter_registry_rejects_invalid_entry_point(monkeypatch):
    entry_point = SimpleNamespace(name="invalid", value="example:invalid", load=lambda: object())
    monkeypatch.setattr(
        converters_module.importlib.metadata,
        "entry_points",
        lambda **selection: (entry_point,),
    )

    with pytest.raises(TypeError, match="entry point 'invalid' must load a Converter"):
        ConverterRegistry().get("source", "target")


def test_zarr_to_xarray_passes_source_credentials_and_conversion_options(monkeypatch):
    calls = []
    fake_xarray = SimpleNamespace(open_zarr=lambda source, **options: calls.append((source, options)) or "dataset")
    monkeypatch.setattr(xarray_zarr_module, "import_module", lambda name: fake_xarray)

    result = DEFAULT_CONVERTERS.convert(
        "zarr",
        "xarray",
        "s3://bucket/weather.zarr",
        source_options={"profile": "weather-reader"},
        conversion_options={"group": "forecast", "chunks": None},
    )

    assert result == "dataset"
    assert calls == [
        (
            "s3://bucket/weather.zarr",
            {
                "group": "forecast",
                "chunks": None,
                "storage_options": {"profile": "weather-reader"},
            },
        )
    ]


def test_xarray_to_zarr_passes_target_credentials_and_conversion_options(monkeypatch):
    calls = []

    class Dataset:
        def to_zarr(self, **options):
            calls.append(options)
            return "store"

    fake_xarray = SimpleNamespace(Dataset=Dataset, DataArray=type("DataArray", (), {}))
    monkeypatch.setattr(xarray_zarr_module, "import_module", lambda name: fake_xarray)

    result = DEFAULT_CONVERTERS.convert(
        "xarray",
        "zarr",
        Dataset(),
        "s3://bucket/weather.zarr",
        target_options={"profile": "weather-writer"},
        conversion_options={"mode": "w-", "zarr_format": 3},
    )

    assert result == "store"
    assert calls == [
        {
            "store": "s3://bucket/weather.zarr",
            "mode": "w-",
            "zarr_format": 3,
            "storage_options": {"profile": "weather-writer"},
        }
    ]


def test_xarray_converter_reports_optional_dependency(monkeypatch):
    def unavailable(name):
        raise ModuleNotFoundError(name)

    monkeypatch.setattr(xarray_zarr_module, "import_module", unavailable)

    with pytest.raises(ImportError, match=r"fsspec-data\[xarray\]"):
        DEFAULT_CONVERTERS.convert("zarr", "xarray", "memory://weather.zarr")


def test_xarray_zarr_round_trip():
    xarray = pytest.importorskip("xarray")
    pytest.importorskip("zarr")
    source = xarray.Dataset(
        {"temperature": (("x", "y"), [[1.0, 2.0], [3.0, 4.0]])},
        coords={"x": [10, 20], "y": [30, 40]},
        attrs={"units": "K"},
    )
    store = fsspec.get_mapper("memory://weather.zarr")

    DEFAULT_CONVERTERS.convert(
        "xarray",
        "zarr",
        source,
        store,
        conversion_options={"mode": "w", "zarr_format": 3, "consolidated": False},
    )
    result = DEFAULT_CONVERTERS.convert(
        "zarr",
        "xarray",
        store,
        conversion_options={"chunks": None, "consolidated": False},
    )

    xarray.testing.assert_identical(result, source)
