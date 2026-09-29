FROM python:3.12-slim
# Rasterio's bundled GDAL libraries still link to the system Expat library.
RUN apt-get update && apt-get install -y --no-install-recommends libexpat1 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY pyproject.toml README.md ./
COPY src ./src
RUN pip install --no-cache-dir . && python -c "import rasterio; from rasterio.merge import merge; print('Rasterio', rasterio.__version__)" && useradd --create-home planner && mkdir /data && chown planner:planner /data
USER planner
ENV RF_PLANNER_DATA=/data
EXPOSE 8000
CMD ["rf-router-web", "--host", "0.0.0.0", "--port", "8000"]
