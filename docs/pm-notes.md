# Projet : orchestrateur perso d'agents de code (ticket → merge)

> Notes PM — interview en cours (2026-09-22). Fichier vivant.

## Pitch
Outil CLI/TUI, binaire unique, qui orchestre N agents de code sur N tickets,
de la définition du ticket jusqu'au merge. Objectif n°1 : réduire la charge
mentale, pas le débit brut. Perso, portable, agnostique de tout employeur,
potentiellement open source.

## Décisions actées

### A. Vision
- A1. Objectif primaire : charge mentale.
- A2. Utilisateur : Jérémy seul, OSS possible → zéro couplage Zeenea/Actian.
      Intégrations = adapters/plugins facilement extensibles.
- A3. N tickets en parallèle, pas de limite câblée.
- A4. Développement itératif, vibecode-friendly (petit cœur, modules simples).
- A5. Paysage scanné : Contrabass / Vibe Kanban / Claude Squad / Uzi / Crystal /
      Sidecar couvrent worktree+parallélisme mais PAS : machine à états
      paramétrable + gates humains stricts sur actions publiques + deps entre
      tickets + budget tokens. → on construit, en volant l'anatomie
      worktree(+tmux?) des existants.

### B. Sources de travail
- B6. Multi-sources → interface `TicketProvider` (Jira, GitHub Issues, local, …).
- B7. Grooming assisté par agent : hors MVP, mais état réservé dans le workflow.
- B8. Triage humain. CONTRAINTE DURE : filtre assignee — ne jamais toucher un
      ticket assigné à quelqu'un d'autre. Chaque provider doit exposer assignee.
- B9. Dépendances entre tickets gérées (bloqué-par ; base de branche = branche
      d'un autre ticket). Le graphe vit dans l'orchestrateur, les providers
      peuvent l'alimenter (Jira "blocks") mais pas le définir.
- B10. Multi-repo. (À confirmer : un ticket = un repo, ou cross-repo ?)

### C. Agents
- C11. omp en premier backend ; interface `AgentRunner` pour rester souple.
- C13. Autonomie paramétrable PAR ÉTAT du workflow ET PAR TICKET (override).
      → cœur du produit = machine à états par ticket, politique de gate par
      transition : auto / notifier / bloquer-jusqu'à-validation.
- C14. Feedback TUI + notifications macOS.
- C15. Tracking coût tokens par ticket : oui.
- C16. Agents en compétition sur un ticket : hors scope MVP, ne pas l'interdire
      par l'architecture.

### D. Cycle de vie & git
- D17. Flow cible décrit : affectation Jira (soi-même ou par autrui) →
  agent développe → agent crée la PR → agent surveille la CI → attente des
  retours (humains OU bots) → traitement des retours si besoin → quand la PR
  est "mergeable" selon des CRITÈRES PARAMÉTRABLES → merge (déclenché par
  l'humain).
  → Concept produit : "mergeability policy" par repo/ticket — prédicat
    composable (CI verte, N approvals, pas de changes-requested, pas de
    conflit, checks nommés…) évalué en continu ; la TUI affiche
    mergeable/pas-mergeable + raisons.
  → Frictions principales : PAS ENCORE RÉPONDU (relancé).
- D18. Conventions (branche, commit, base) : PARAMÉTRABLE — l'outil doit
  survivre aux changements de job. → config par repo (conventions de nommage
  templatées, ex. `{type}/{ticket}-{slug}`, branche de base, format commit),
  avec défauts globaux sensés. Zéro convention câblée en dur.
- D19. Gates 3 niveaux VALIDÉ : 🟢 local (édits/commits/tests) = auto ;
  🟠 push branche feature = auto + notification ; 🔴 action publique
  (PR create/edit, commentaires, merge, transitions tracker) = validation
  explicite à chaque fois, jamais mémorisée. Configurable (outil agnostique),
  le profil perso de Jérémy verrouille le 🔴.
- D20. Pré-passe agent-reviewer : PAS prioritaire. À garder en tête (design :
  simple état optionnel du workflow, pas de structure dédiée).
- D21. Retours de PR = option (c) : distinction bot/humain. Bot (linter,
  coverage…) → l'agent traite directement en 🟢/🟠. Humain → l'agent prépare
  fix + réponse, Jérémy valide avant push/réponse (défaut (b), à confirmer
  à l'usage).
- D22. Merge TOUJOURS manuel (keypress TUI quand mergeable=true). Pas de cas
  d'auto-merge.
- D23. Base qui avance : l'agent REBASE SEUL si trivial (aucun conflit) →
  🟠 push --force-with-lease + notif. Conflits → escalade : l'agent propose
  sa résolution, Jérémy valide. (Rebase par défaut, paramétrable
  rebase/merge par repo.)
- D24. Transitions tracker : l'outil les PROPOSE au bon moment du workflow
  (pré-remplies, un keypress pour accepter), exécution = 🔴 validation
  explicite. Dans le MVP.

### E. Vérification
- E25. Pas d'avis tranché → défaut : commande(s) de check locales optionnelles
  par repo (build/tests rapides) avant création de PR ; la CI reste la source
  de vérité. Rien d'imposé.
- E26. Boucle CI rouge → fix : l'agent lit les logs, corrige, repousse (🟠).
  Plafond 2-3 tentatives (paramétrable) OU budget tokens, puis escalade :
  la TUI montre le diagnostic de l'agent.
- E27. Isolation des environnements locaux (ports, DB, caches) : HORS MVP.
  Prévoir le point d'extension dès le début : hook `setup-worktree` /
  `teardown-worktree` paramétrable par repo — vide par défaut.

### F. Forme du produit
- F28. Deux surfaces sur le même cœur : TUI dashboard (tickets × état × agent
  × mergeable, drill-down logs/diff/validations 🔴) + CLI scriptable
  (`add`, `status --json`, `approve`, …).
- F29. Daemon superviseur lancé à la demande (pas un service système) ;
  la TUI s'attache/se détache ; les agents survivent à la fermeture du
  terminal.
- F30. Persistance SQLite locale : tickets, états, worktrees, budgets,
  event log d'audit. Reprise après crash/reboot ; agents relançables depuis
  leur dernier état.
- F30bis. SYNCHRO AUTO avec les sources : le SQLite local est une projection,
  jamais la vérité pour les données distantes. Le daemon poll/rafraîchit
  périodiquement tickets (Jira/GitHub Issues), PRs, reviews, checks CI ;
  détecte les changements externes (ticket réassigné, PR commentée, CI finie)
  et déclenche les transitions/notifications. Vérité locale = worktrees,
  états du workflow, event log ; vérité distante = tickets/PRs/CI.
- F31. "Simple binaire" = UN SEUL binaire compilé, c'est tout. Les outils
  externes (git, backends agents) sont acceptables. Reco maintenue : API HTTP
  directes pour GitHub/Jira (portabilité entre jobs) plutôt que gh/acli —
  non dogmatique.
- F32. Langage : pas d'enjeu pour Jérémy, faible pour Rust. → RUST retenu
  (ratatui + rusqlite + reqwest ; binaire statique multiplateforme).
- F33. ABSOLUMENT multiplateforme. Conséquences : pas de dépendance tmux
  (gestion de process agents portable), notifications/keychain/chemins
  abstraits par plateforme. À confirmer : Windows inclus ?
- Le projet n'a pas encore de nom.
- F33bis. Cibles : macOS + Linux natifs ; Windows via WSL (couvert par la
  cible Linux). Windows natif "un jour peut-être" — ne dicte pas le design.

### G. Sécurité & confidentialité
- G34. Secrets : keychain natif par OS (crate `keyring` : Keychain /
  libsecret / Credential Manager) + fallback fichier chmod 600 + env vars
  pour headless.
- G35. ZÉRO appel LLM direct dans l'orchestrateur : toute intelligence passe
  par les backends agents (omp, …). L'orchestrateur reste un outil
  déterministe.
- G36. Event log append-only par ticket VALIDÉ : qui a fait/validé/publié
  quoi, quand. Alimente aussi l'affichage d'historique TUI.

### H. Cadrage final
- H37. Découpage MVP validé, incréments utilisables seuls :
  1. Socle : config (providers, repos, conventions), SQLite + event log,
     daemon + CLI `status`.
  2. Ticket → PR : `add <ticket>` → worktree → agent omp → PR draft,
     gates 🟢🟠🔴.
  3. TUI dashboard : états, validations 🔴 en attente, logs agents.
  4. Boucles : sync auto, CI-watch + retry, retours review bot/humain,
     mergeable-policy + merge manuel.
  5. Confort : transitions tracker proposées, notifications OS, budgets
     tokens, dépendances entre tickets.
- H38. Anti-objectifs confirmés : pas de web-UI ; pas de mode serveur
  multi-utilisateurs ; on ne réimplémente JAMAIS un agent (les backends
  restent des boîtes noires pilotées).
- H39. Pas de métrique formelle ("osef") — succès = usage réel constaté.
- D17bis. Jamais répondu ; hypothèse retenue (non contredite) : frictions =
  polling multi-PR, relance d'agent avec contexte, état mental de N tickets.

## Interview close 2026-09-22 — prochaines étapes
1. Trouver un nom.
2. Rédiger la spec de kickoff (architecture, modèle de données, machine à
   états, interfaces TicketProvider/AgentRunner/Forge) depuis ces notes.
3. Scaffolder le repo Rust (workspace : core / daemon / tui / cli).

## Questions ouvertes (en attente)

## Suggestions PM émises (à valider)
- Gates 3 niveaux : vert = worktree local libre ; orange = push branche feature
  auto ; rouge = toute action publique/visible (PR body, commentaires, merge,
  transitions tracker) → validation explicite à chaque fois. Encode la règle
  personnelle « jamais publier sous mon identité sans accord préalable ».
- Étape "grooming par agent" (plan + questions ouvertes avant tout code) —
  meilleur ROI connu, phase 2.
- Event log append-only par ticket dès le début (quasi gratuit tôt).
- Daemon + TUI qui s'attache (sinon perte de travail à la fermeture du terminal).

## Addendum 2026-09-22 — post-kickoff
- Repo créé : https://github.com/pjerem/yardmaster (public, MIT). 24 issues / 5 milestones.
- Autonomie déléguée à l'orchestrateur (cette session puis yardmaster lui-même) :
  commits/push/merge autorisés sur CE repo, merge conditionné à CI VERTE.
  Les actions publiques ailleurs restent 🔴.
- Exigence de test : BDD (Gherkin/cucumber) au niveau du binaire `yard` pour
  les critères d'acceptation ; tests unitaires par module en complément.
- Binaire nommé `yard` ; identité commits = Jérémy P. <72425+pjerem@users.noreply.github.com>.
