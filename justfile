@profile-indexer:
    sudo rm -rf data/index
    cargo flamegraph --root -- indexer local configs/indexer/profile.toml

@webgraph:
    rm -rf data/webgraph
    cargo run --release -- webgraph local configs/webgraph/local.toml

@frontend-rerun *ARGS:
    cd frontend; npm run build
    ./scripts/run_frontend.py {{ARGS}}

@frontend *ARGS:
    cd frontend; npm install
    cargo watch -s 'just frontend-rerun {{ARGS}}'

@astro:
    cd frontend; npm run dev

@configure *ARGS:
    just setup_python_env
    ./scripts/export_crossencoder
    ./scripts/export_qa_model
    ./scripts/export_abstractive_summary_model
    ./scripts/export_dual_encoder
    ./scripts/export_fact_model
    cd frontend; npm install; npm run build
    cargo run --release --all-features -- configure {{ARGS}}

@centrality webgraph output:
    ./scripts/build_harmonic {{webgraph}} {{output}}


@entity:
    rm -rf data/entity
    cargo run --release -- indexer entity data/enwiki_subset.xml.bz2 data/entity

@setup_python_env:
    rm -rf .venv
    python3 -m venv .venv
    ln -s .venv/lib/python3* .venv/lib/python3
    .venv/bin/pip install -r scripts/requirements.txt