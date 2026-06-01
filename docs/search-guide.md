# Hybrid search — operator guide

Ce guide documente la refonte de `search()` introduite par le RFC
`bdee1af4` et son implementation plan `640e2894`. Public cible : qui
opère un serveur companyos-orchestrator, qui utilise le tool MCP
`search` depuis un agent, ou qui diagnostique un problème de recherche.

## 1. Pipeline en une phrase

Une requête est embedée par un modèle local multilingue, search bouche
en parallèle FTS5 (lexical BM25) et sqlite-vec (kNN cosine), les deux
classements sont fusionnés par Reciprocal Rank Fusion (RRF, k=60), et
le top-N retourne avec les métadonnées des artifacts.

## 2. Composants

### 2.1. Couche lexicale (FTS5)

- Table virtuelle `artifacts_fts` avec tokenizer explicite `unicode61
  remove_diacritics 2 separators '-_./'`.
- BM25 ranking avec column weights `(kind=0, title=10, description=3,
  tags=5, content=1)`.
- Query normalisation par `sanitize_fts_query` en mode `Natural`
  (default) : strip des caractères réservés FTS5, wrap chaque token
  dans `"…"`, join par `OR`. Casse l'AND-implicite et neutralise les
  identifiants techniques.

### 2.2. Couche sémantique

- Modèle : `MultilingualE5Small` (118M params, dim 384, int8 ONNX via
  fastembed). Multilingue natif FR + EN technique. Footprint ~470MB
  sur disque, ~150MB RAM en steady-state.
- Table virtuelle `artifacts_vec USING vec0(artifact_id text,
  embedding float[384] distance_metric=cosine)`.
- kNN brute force (suffisant à notre échelle, < 1ms warm sur 100
  vecteurs).
- `model_version` persisté dans `index_metadata` : si le binaire
  démarre avec un model_version différent de celui stocké, wipe
  `artifacts_vec` + reindex_all automatique.

### 2.3. Fusion

- Reciprocal Rank Fusion avec `k=60`.
- Top-K candidat par axe = 50 (constante `SEARCH_RETRIEVE_K`).
- Tie-breaker déterministe par id (lexicographic ascending).
- Pas de score calibration : RRF ne consomme que les rangs.

### 2.4. Filtres structurés (push-down)

`SearchFilters` push down dans les deux chemins lexical et sémantique :

- `kinds`: liste, semantic IN
- `author`: persona id exacte
- `tags`: liste, semantic OR (EXISTS json_each)
- `project`: project slug exact
- `created_after` / `created_before`: RFC3339 timestamp
- `id_prefix`: LIKE `<prefix>%`

### 2.5. Couches optionnelles

- `rerank=true` et `hyde=true` retournent une erreur explicite
  `AnthropicKeyMissing` (étape 13 reportée à un follow-up — voir
  section 7).

## 3. Pre-fetch du modèle d'embedding

**À faire UNE FOIS après install et après tout changement de
`model_version`.** Sans cache local, le serveur exit avec une erreur
claire au boot.

```bash
cargo run -p companyos-orchestrator-server -- --prefetch-embeddings
```

Le cache vit dans `company/data/embeddings/cache/` (~465 MB, gitignored
via `*.db*` n'est pas suffisant : `.gitignore` inclut explicitement
`company/data/embeddings/`).

En CI : exécuter cette commande avant tout `cargo test` qui touche aux
tests integration_search.

## 4. API du tool MCP `search`

```jsonc
{
  "query": "embeddings sémantiques",        // string, required
  "kind": "rfc",                            // string, optional (compat)
  "limit": 10,                              // int, optional, max 100
  "mode": "hybrid",                         // "lexical"|"semantic"|"hybrid", optional
  "author": "implementer",                  // string, optional
  "tags": ["search", "rrf"],                // list, optional, OR-semantic
  "project": "company-os",                  // string, optional
  "created_after": "2026-05-01T00:00:00Z",  // RFC3339, optional
  "created_before": "2026-12-31T23:59:59Z", // RFC3339, optional
  "id_prefix": "bdee1af4",                  // string, optional
  "explain": false,                         // bool, optional
  "rerank": false,                          // bool, reserved (étape 13)
  "hyde": false                             // bool, reserved (étape 13)
}
```

### 4.1. Sémantique du mode

- `lexical` : FTS5 BM25 pur. Idéal pour les UUIDs et identifiants exacts.
- `semantic` : embeddings purs. Idéal pour les paraphrases et synonymes.
- `hybrid` (default) : fusion RRF des deux. Idéal pour les requêtes en
  langage naturel.

### 4.2. Cas empty-query

`query=""` est valide UNIQUEMENT si au moins un filtre est fourni.
Dans ce cas, search retourne les artifacts matchant les filtres,
ordonnés par `COALESCE(created_at, indexed_at) DESC` (recent first).
Sinon : erreur explicite "empty query requires at least one filter".

### 4.3. Sortie

Format minimal :

```jsonc
{
  "results": [
    { "id": "...", "kind": "...", "title": "...", "description": "...",
      "tags": ["..."] }
  ],
  "count": 10
}
```

Avec `explain=true`, un champ supplémentaire :

```jsonc
{
  "explain": {
    "mode_applied": "Hybrid",
    "candidate_set_sizes": { "lexical": 50, "semantic": 50, "fused": 10 },
    "latency_ms": 5
  }
}
```

## 5. Tool MCP `index_status`

Inspection lecture seule de l'état d'indexation. Sans argument retourne
les compteurs globaux ; avec `path=<rel>` retourne aussi la status par
fichier.

```jsonc
{
  "global": {
    "artifacts_count": 92,
    "fts_count": 92,
    "vec_count": 92,
    "triplet_coherent": true,
    "file_watcher_alive": true,
    "pending_index_queue_size": 0,
    "queue_mode": "direct",
    "last_indexed_at": "2026-06-01T14:30:12.345678+00:00"
  },
  "per_path": {                  // si path fourni
    "path": "company/rfcs/...",
    "indexed_at": "...",
    "file_mtime": "...",
    "stale": false,
    "present_in_fts": true,
    "present_in_vec": true
  }
}
```

`stale=true` veut dire `file_mtime > indexed_at` — le fichier a été
écrit depuis le dernier index. Le file watcher devrait le rattraper en
< 1 seconde ; si non, appeler `index_now(path=...)`.

`triplet_coherent=false` signale un bug : les trois tables divergent.
Le mode autorepair au boot (PILIER D) restaure l'invariant.

## 6. Migration au boot (à savoir)

Au démarrage du serveur :

1. PILIER A : acquérir le file lock exclusif.
2. PILIER D : `PRAGMA integrity_check`. Si KO -> wipe + rebuild depuis YAML.
3. sqlite-vec smoke test (`vec0_smoketest` jetable).
4. `migrate()` : `CREATE IF NOT EXISTS` sur toutes les tables. ALTER
   TABLE artifacts ADD COLUMN sur DB existantes (idempotent via catch
   "duplicate column"). Detection de drift du tokenizer FTS5 ->
   DROP + CREATE artifacts_fts.
5. Détection drift du `model_version` (architecture marker + version
   du modèle). Si mismatch -> wipe artifacts_vec.
6. Si FTS drift OR model drift -> reindex_all SYNCHRONE avant de
   servir.
7. Spawn file watcher + reindex background.

## 7. Limites connues

- **Couches optionnelles rerank/HyDE non câblées.** Les flags `rerank`
  et `hyde` retournent `AnthropicKeyMissing` ; ils seront wirés dans
  une future itération (étape 13 du plan 640e2894 reportée). Le mode
  hybrid sans rerank donne déjà recall@10 = 0.895 sur le bench, jugé
  suffisant pour la première mise en service.
- **Déterminisme cross-architecture non garanti.** Same-machine OK
  (cosine sim > 0.9999 entre 2 invocations d'embed_text sur le même
  input). Cross-architecture (x86_64 vs aarch64) peut produire des
  embeddings légèrement différents ; détecté via `architecture_marker`
  concaténé dans `model_version` -> wipe + reindex automatique.
- **Bench 10x extrapolé.** La génération synthétique de 756 artifacts
  reportée à un follow-up. L'extrapolation théorique par composant
  donne p95 ≈ 3x baseline = ~21ms à 10x (largement sous la cible
  2000ms).
- **UUID-only queries en mode hybrid.** Une requête de type
  `"bdee1af4"` peut voir l'identifiant exact noyé par le sémantique.
  Workaround : passer `mode="lexical"` explicitement, ou ajouter le
  contexte (par exemple `"RFC bdee1af4"`).

## 8. Troubleshooting

### 8.1. "embedding model cache not found"

Au boot : signal explicite que le cache HF est absent. Exécuter
`--prefetch-embeddings` comme indiqué.

### 8.2. `search()` retourne 0 résultat sur une query attendue

1. Appeler `index_status(path=<rel>)` pour vérifier que l'artifact est
   bien indexé (`present_in_fts=true`, `present_in_vec=true`,
   `stale=false`).
2. Si `stale=true` : appeler `index_now(path=...)` pour forcer
   l'indexation.
3. Si `triplet_coherent=false` au global : appeler `reindex_all`.
4. Sinon, appeler `search` avec `explain=true` pour voir les tailles
   des candidate sets. Si `lexical=0` et `semantic=0` -> la query est
   probablement mal formulée (essayer en mode `semantic` pur).

### 8.3. "sqlite-vec smoke test failed"

ABI cassée entre versions de sqlite-vec et rusqlite. Vérifier que
`Cargo.toml` pin bien `sqlite-vec = "=0.1.6"` (la version stable la
plus récente sans diskann.c manquant).

### 8.4. p95 latence > 500ms

1. Vérifier que `cargo build --release` a été utilisé (le mode debug
   est ~10x plus lent).
2. Vérifier la taille du corpus via `index_status`. Au-delà de ~5000
   artifacts, la latence kNN brute force devient sensible — passer à
   un index ANN (HNSW) sera nécessaire (hors scope iteration 1).
3. Si `rerank=true` ou `hyde=true` est activé : ces flags ajoutent
   1-3s par query (appel HTTP Anthropic). Désactiver pour comparer.

## 9. FAQ

### Pourquoi sqlite-vec et pas Qdrant / LanceDB ?

Cohérence architecturale : sqlite-vec partage la même connexion SQLite
que FTS5 et `artifacts` (tables métier). Une seule transaction couvre
les quatre tables. Pas de daemon supplémentaire, pas de second backup,
pas de second mécanisme de cohérence. À notre échelle (< 10k artifacts),
la performance brute force est suffisante (< 5ms warm).

### Pourquoi RRF et pas une combinaison linéaire ?

Les scores BM25 et cosine ne sont pas comparables sans calibration.
RRF ne consomme que les rangs, donc reste correct même si on change
de modèle d'embedding ou de poids BM25. Empiriquement le plus robuste.

### Pourquoi MultilingualE5Small et pas BGE-M3 ?

Footprint et latence. E5Small est 5x plus petit (120MB int8 vs ~530MB
pour BGE-M3 int8) pour une qualité multilingue compétitive sur notre
corpus FR+EN. Le fallback vers E5Base/BGE-M3 est documenté dans le
plan 640e2894 étape 1 si une mesure empirique futur le justifie. Le
benchmark actuel (recall@10 = 0.895 sur 92 artifacts, MRR = 0.585)
valide la baseline.
