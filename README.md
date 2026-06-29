# cc-sonde

Application Rust de monitoring HTTP et d'auto-scaling pilote par des metriques Warp 10. Concue pour tourner sur **Clever Cloud** mais utilisable partout.

## Fonctionnalites

- **Healthcheck probes** — surveillance periodique d'endpoints HTTP (status, body, regex, headers) avec execution d'une commande en cas d'echec repete
- **WarpScript probes** — requetes Warp 10 avec auto-scaling level-based (flavors + instances) ou stateless (webhook/alerte a chaque depassement de seuil)
- **Multi-metric** — scale UP si un seuil est depasse (OR), scale DOWN si tous sont en dessous (AND)
- **Multi-instance** — verrou distribue Redis pour eviter les actions en doublon entre replicas
- **Dry run** — validation de configuration sans effets de bord (`--dry-run`)
- **Persistance** — in-memory (defaut) ou Redis (avec `--features redis-persistence`)

## Quickstart

```bash
# Build
cargo build --release
# ou avec Redis
cargo build --release --features redis-persistence

# Lancer
./target/release/cc-sonde --config config.toml

# Avec le endpoint de liveness
./target/release/cc-sonde --config config.toml --healthcheck
```

Voir [`INSTALL.md`](INSTALL.md) pour la documentation complete (configuration, variables d'environnement, WarpScript, troubleshooting, etc.).

## Deploiement sur Clever Cloud

### Pre-requis

- Une application **Docker** ou **Rust** sur Clever Cloud
- (Optionnel) Un add-on **Redis** si vous utilisez la persistance Redis ou le mode multi-instance

### Variables d'environnement

Configurez ces variables dans le panneau de l'application Clever Cloud :

| Variable | Obligatoire | Description |
|----------|-------------|-------------|
| `WARP_ENDPOINT` | si WarpScript probes | URL de l'API exec Warp 10 |
| `WARP_TOKEN` | non | Token de lecture Warp 10 (fallback global) |
| `REDIS_URL` | non | URL Redis (fournie automatiquement par l'add-on Redis) |
| `MULTI_INSTANCE` | non | `true` pour le mode multi-instance (requiert Redis) |
| `RUST_LOG` | non | Niveau de log (`info` par defaut) |
| `CC_RUN_COMMAND` | oui | Commande de lancement (voir ci-dessous) |

### Commande de lancement

Dans `CC_RUN_COMMAND` (ou dans le fichier de run de votre application) :

```bash
./target/release/cc-sonde --config config.toml --healthcheck --healthcheck-port 8080
```

Le port `8080` est le port par defaut expose par Clever Cloud. Le endpoint `/` repond `200 OK` et sert de health check pour la plateforme.

Pour le mode multi-instance avec Redis :

```bash
./target/release/cc-sonde --config config.toml --healthcheck --multi-instance
```

### Arret gracieux

Clever Cloud envoie un `SIGTERM` avant de stopper une instance. cc-sonde intercepte ce signal et termine proprement les taches en cours. Le timeout d'arret est configurable via `--shutdown-timeout` (defaut : 2s).

### Add-on Redis

Si vous ajoutez un add-on Redis a votre application, Clever Cloud injecte automatiquement `REDIS_URL` dans l'environnement. cc-sonde l'utilise directement — aucune configuration supplementaire n'est necessaire.

Pour activer la persistance Redis, le binaire doit etre compile avec `--features redis-persistence`.

### Exemple de configuration pour Clever Cloud

```toml
# Healthcheck d'une app Clever Cloud
[[healthcheck_probes]]
name = "Mon App"
interval_seconds = 60
on_failure_command = "clever restart --app ${APP_ID}"
failure_retries_before_command = 2

[healthcheck_probes.checks]
expected_status = 200

[[healthcheck_probes.apps]]
id = "app_xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
url = "https://mon-app.cleverapps.io/health"
```

```toml
# Auto-scaling WarpScript
[[warpscript_probes]]
name = "CPU Scaler"
warpscript_file = {cpu = "warpscript/cpu.mc2"}
interval_seconds = 60

[[warpscript_probes.apps]]
id = "app_xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[warpscript_probes.scaling]
instances = {min = 1, max = 3}
flavors = ["S", "M", "L"]
scale_up_threshold = {cpu = 70.0}
scale_down_threshold = {cpu = 40.0}
upscale_command = "clever scale --app ${APP_ID} --flavor ${FLAVOR} --instances ${INSTANCES}"
downscale_command = "clever scale --app ${APP_ID} --flavor ${FLAVOR} --instances ${INSTANCES}"
```

## Documentation complete

Toute la documentation detaillee (parametres de configuration, WarpScript, persistance, multi-instance, troubleshooting, securite) se trouve dans [`INSTALL.md`](INSTALL.md).

## License

MIT
