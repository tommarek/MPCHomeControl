# MPCHomeControl
Model predictive control of a house heating/cooling and possibly other things. This is a pet project and the goal here is to improve temperature comfort and reduce heating costs in our house.

## Documentation
 - [`docs/configuration.md`](docs/configuration.md) — how to write `model.json5` and `config.json5`: every field, its units, and tips (start here to describe a house).
 - [`theory.md`](theory.md) — the thermal RC-network physics behind the model.
 - [`docs/api.md`](docs/api.md) — the read-only monitoring/reporting API and dashboard.
 - [`docs/controllers.md`](docs/controllers.md) — the universal, language-agnostic controller protocol and the Growatt / heating reference controllers (in `controllers/`).


# my materials
 - https://www.heluz.cz/files/HELUZ-UNI-30-brousena_2022.pdf
 - https://www.heluz.cz/files/HELUZ-FAMILY-44-2in1-brousena_technicky-list_CZ.pdf
 - https://www.heluz.cz/files/HELUZ-AKU-Z-17_5-brousena_2022.pdf
 - https://www.heluz.cz/files/HELUZ-17_5-brousena_2022_09.pdf