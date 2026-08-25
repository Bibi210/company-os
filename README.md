# CompanyOS

**Une organisation d'agents LLM qui se construit en s'utilisant elle-même, gouvernée par un harness mécanique.**

CompanyOS est une "entreprise logicielle" autonome : quatre agents (PM, Architect, Implementer, CEO) collaborent selon un processus formel de revue et de gouvernance pour concevoir, implémenter et faire évoluer des projets, y compris CompanyOS lui-même. Chaque évolution du système passe par ses propres règles : task-request, RFC, revue triaxiale, approbation, permis d'écriture, implémentation, capitalisation.

La particularité du projet n'est pas l'orchestration multi-agents. C'est le **harness** : les règles importantes ne vivent pas dans des prompts que les modèles peuvent ignorer, elles vivent dans du code qui les rend impossibles à contourner.

---

## Principes directeurs

**1. Le harness avant la prose.**
Une règle écrite dans un prompt est un vœu. Une règle encodée dans un hook, un schema JSON, un tool MCP ou un pre-commit est un fait. Test directeur du projet : si la mémoire YAML était effacée demain (`make clean-company`), tout ce qui a été appris doit survivre, parce que l'appris vit dans le mécanisme, pas dans le texte.

**2. Le LLM décide, l'algorithme tient les registres.**
Les transitions de cycle de vie (RFC approuvé, permit scellé, statuts synchronisés, indexation) sont algorithmiques et automatiques. Le jugement (architecture, curation, arbitrage) reste aux agents. Aucun mécanisme ne retire un point de décision légitime ; aucun agent ne fait du bookkeeping qu'une machine sait faire.

**3. L'invariant mécanique.**
On ne corrige pas un anti-pattern par une règle d'usage : on retire le paramètre, on ferme le schema, on rejette côté serveur. Même principe au niveau du code : les invariants vivent dans les types, jamais dans des conventions de contenu de String.

---

## Architecture

```
company-os/
├── company/               Le système lui-même
│   ├── personas/          Contrats des 4 agents (PM, Architect, Implementer, CEO)
│   ├── schemas/           14 JSON Schemas, verrouillés (unevaluatedProperties)
│   ├── config/            Règles partagées, protocole de review, zones protégées
│   ├── plugins/           Harness JS : defense-in-depth (hooks), mcp-proxy (supervision)
│   ├── rfcs/              Request For Change (39+ RFCs, tous cycle complet)
│   ├── lessons/           Mémoire collective (60+ lessons, graphe chaîné)
│   ├── roadmaps/          Suivi des domaines, statuts auto-synchronisés
│   └── scripts/           Outillage d'enforcement (zone protégée)
├── crates/                Le serveur Rust
│   ├── orchestrator/      Engine : index hybride, review rounds, write permits
│   ├── mcp-servers/       Serveurs MCP (orchestrator, yaml-validator)
│   ├── validation/        Validation schema + placement kind/chemin
│   └── config/            Chargement de la config, watcher, zones protégées
├── projects/              Les projets gérés (task-requests, design-docs, plans...)
├── .githooks/             Pre-commit : validation, permits, make ci (zone protégée)
└── tests/                 Tests d'intégration workspace
```

### Le harness en trois couches

| Couche | Mécanisme | Ce qu'il rend impossible |
|---|---|---|
| Hooks (plugin JS) | Interception write/edit/bash, revert automatique | Écrire en zone protégée sans permis nominatif, écrire pour le compte d'un autre agent, écritures du CEO |
| Serveur (Rust, MCP) | Gardes dans les tools | Auto-review, vote approve avec findings, permis sans RFC approuvé, consume avec worktree sale, reviewers sous le minimum du protocole, grant sur fichier de gouvernance sans approbation utilisateur confirmée |
| Pre-commit (git) | Validation YAML, audit des permits, `make ci` bloquant | Committer un artifact invalide ou mal placé, committer en zone protégée sans permis, merger du code rouge |

### Le workflow

```
task-request ──▶ design-doc / RFC ──▶ review round ──▶ approbation CEO
     ▲                                (triaxial,           │
     │                                 3 reviewers,        ▼
  lesson-learned ◀── implémentation ◀── write permit (scellé en git,
  (mémoire chaînée)   (plan reviewé)     nominatif, pré-check de périmètre)
```

Chaque review analyse trois axes obligatoires (nominal, négatif, edge cases), forme vérifiée par schema. Un reviewer ne peut pas approuver avec des findings correctifs : la contradiction est rejetée par le serveur. L'auteur ne peut pas se reviewer : rejeté par le serveur. Les permis d'écriture sont émis par le CEO sur RFC approuvé uniquement (vérifié mécaniquement), couvrent des chemins précis, sont liés à leur bénéficiaire, et leur périmètre est comparé automatiquement aux fichiers annoncés par le RFC.

### La mémoire collective

Les artifacts YAML sont la source de vérité, indexés automatiquement dans SQLite (FTS5 BM25 + embeddings locaux déterministes + fusion RRF, rappel mesuré 0.895). Les lessons-learned forment un graphe chaîné (supersedes, derived-from, related) avec détection mécanique des liens pendants et des supersessions asymétriques. L'index est un cache : il se reconstruit intégralement depuis les YAML, permits inclus.

### Résilience

Les binaires servis vivent dans `target/serve/`, jamais touchés par les builds : la mise à jour du serveur est une promotion atomique explicite (`make deploy-serve`). Le proxy MCP supervise le serveur sans état terminal : backoff avec réarmement, respawn conditionné à un binaire présent, buffer FIFO transparent pendant les indisponibilités. Un agent ne fait jamais de retry : au pire, il reçoit une erreur structurée qui prescrit l'escalade humaine.

---

## Démarrage

Prérequis : Rust stable, Node.js 20+, git, [opencode](https://opencode.ai).

```bash
make setup          # build release + CI + promotion des binaires servis
opencode            # démarre une session : le PM est l'interface unique
```

Commandes utiles :

```bash
make ci             # fmt + clippy + tests Rust + tests JS + validation YAML + naming
make deploy-serve   # promotion atomique des binaires MCP vers target/serve/
make validate       # validation schema de tous les artifacts
make test-js        # tests du harness JS
```

L'utilisateur ne parle qu'au PM. Le PM clarifie l'intention, crée les task-requests et orchestre les autres agents. Les décisions structurantes (zones protégées, personas, schemas) remontent à l'utilisateur via des déclencheurs mécaniques.

---

## État du projet

Programme **V1** en cours : refonte complète du système par lui-même, domaine par domaine (hygiène, mémoire, durcissement des process, personas, schemas, automatisation, audits de code, refactor du harness, tag v1). L'avancement est tracé dans `company/roadmaps/`, chaque étape par un RFC au cycle complet. Sur les 62 règles de process recensées, 27 sont aujourd'hui des garanties mécaniques qui survivraient à l'effacement de la mémoire.

Ce dépôt est à la fois le produit et la démonstration : l'historique git contient l'intégralité des cycles de décision (RFCs, reviews, permits scellés, lessons) qui ont produit chaque ligne.

## Licence

Projet personnel expérimental. Tous droits réservés.
