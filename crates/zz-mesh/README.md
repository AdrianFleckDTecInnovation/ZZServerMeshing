# zz-mesh in ein Bevy-Spiel integrieren

Mit `zz-mesh` 0.1 besitzt jeder Worker eine Bevy-0.15-World. Dein Spiel definiert Komponenten, Simulationssysteme und Spielregeln. Die Bibliothek speichert stabile Objekt-IDs, prüft Schreibrechte und übergibt zusammengehörige Entities als Gruppe an einen anderen Worker.

## Vom Checkout zum ersten Spielstand

1. Lege das Spiel und diesen Checkout nebeneinander. Ergänze im **Spiel** die Abhängigkeiten. Ein bestehendes `bevy = "0.15"` kann die beiden Bevy-Teilcrates ersetzen, wenn du deren Typen aus `bevy::prelude` importierst.

   ```toml
   [dependencies]
   zz-mesh = { path = "../ZZServerMeshing/crates/zz-mesh" }
   bevy_app = "0.15"
   bevy_ecs = "0.15"
   serde = { version = "1", features = ["derive"] }
   ```

2. Übernimm das ausführbare Beispiel als `src/main.rs`. Es registriert eine Komponente, lädt oder erzeugt einen Reaktor, führt einen autoritativen Spielschritt aus und liest seinen gespeicherten Energiewert zurück. [Beispielcode](examples/persistent_game.rs)
3. Führe im Spielverzeichnis **zweimal** `cargo run -- world.sqlite` aus. Eine neue Datei ergibt zuerst `energy=1`, danach `energy=2`. Die Reaktor-ID bleibt gleich. Im Mesh-Checkout lautet derselbe Aufruf `cargo run --locked -p zz-mesh --example persistent_game -- target/world.sqlite`. Rust stable und ein C-Linker reichen. Unter Windows benötigst du die MSVC-Toolchain, C++ Build Tools und das Windows SDK.

Für den Bezug aus deinem Fork verwende im Mesh-Checkout den vorbereiteten Branch.

```sh
git switch feat/bevy-game-integration
git fetch origin
git rebase origin/main
git push -u origin feat/bevy-game-integration
git rev-parse HEAD
```

Ersetze im Spiel den lokalen Pfad durch `zz-mesh = { git = "https://github.com/AdrianFleckDTecInnovation/ZZServerMeshing.git", rev = "<ausgegebener Commit>" }`. Übernimm die aktualisierte `Cargo.lock` ins Spiel-Repo. Im Mesh-Checkout öffnet `cargo doc --locked -p zz-mesh --no-deps --open` die lokale API-Referenz mit dem vollständigen Beispiel.

## Welche Entscheidungen trifft das Spiel?

| Aufgabe | Vertrag mit dem Mesh |
| --- | --- |
| Identität und Zuständigkeit | `Managed::new(object_id, group_id)` verbindet eine dauerhafte ID mit ihrer Übergabegruppe. Speichere Referenzen als `Id`. Ein Bevy-`Entity` gilt nur in der aktuellen World. Lege Objekte mit gegenseitigen Pflichtreferenzen in dieselbe Gruppe. |
| Komponenten | Registriere jeden Typ über `mesh_component` mit fester Schema-ID, Version und Codec. `persist` steuert Checkpoints, `migrate` die Übergabe. `Public` erreicht abonnierende Clients und Worker, `Private` den berechtigten Besitzer, `Workers` abonnierende Worker, `Local` keine Replikation. |
| Spielschritte | Filtere schreibende Systeme mit `With<Simulated>`. Eine `Replica` ist eine entfernte Sicht. `Staged` kennzeichnet vorbereitete Objekte während einer Übergabe. Lokale Ableitungen wie Collider und Bevy-Referenzen baut `mesh_rebuild` aus importierten Komponenten auf. |
| Start und Neustart | Öffne dieselbe Weltdatei, rufe einmal pro Prozess und Slot `start_worker(slot)` auf, installiere `MeshPlugin::persistent` und registriere alle Codecs und Provider. Stelle vor dem ersten Tick die Gruppen aus `store.owned(slot)?` mit `restore_group(app.world_mut(), group)?` wieder her. Erzeuge Startobjekte nur bei `Lookup::Unknown`. `Deleted` ist dauerhaft belegt. |
| Bewegung und Aktionen | Bewegung kann nach einem Ausfall bis zum letzten Checkpoint zurückspringen. `checkpoint_group` nach dem Spielschritt schreibt ausstehende Komponentenänderungen. Verwende für Handel, Abbau und Inventar typisierte `mesh_action`-Handler und `commit_batch`. Wiederhole eine ausstehende Aktion mit derselben Session, Sequenz und Nutzlast. |
| Spielercluster und große Daten | Melde Objekte als `Presence` und Interessenbereiche als `Area` an die `Directory`. Beim Annähern kommen räumliche Abonnements hinzu, beim Trennen laufen sie aus. Getrennte Inseln benötigen weiterhin Kontrollverkehr. `mesh_provider` und `mesh_datasets` verknüpfen Chunks und Baupläne mit geprüften Datenmanifesten. |

## Betrieb und Prüfung

- **Speicher.** Alle Worker-Prozesse einer Welt öffnen dieselbe SQLite-Datei auf einem lokalen Datenträger. WAL sichert bestätigte Transaktionen. Für eine Welt über unabhängige Hosts muss der Speicherzugriff erweitert werden. Sichere eine laufende Welt über das SQLite-Backupverfahren oder fahre alle Worker vor einer vollständigen Dateisicherung sauber herunter. Prüfe die Sicherung durch Öffnen einer Kopie.
- **Kompatibilität.** Der Store erkennt bestehende Weltdateien aus diesem Projekt und markiert ihr Format beim ersten Öffnen. Ein unbekanntes Format bricht den Start ab. Ein geändertes Komponentenformat benötigt eine geplante Datenmigration. Unbekannte Schema-IDs und inkompatible Versionen verhindern die Aktivierung einer Gruppe.
- **Fehler.** Bei `Fenced` hat der Worker seine Schreibberechtigung oder Revision verloren. Ermittle den aktuellen Owner. Bei `Busy` warte vor einer begrenzten Wiederholung. Ein fehlender Codec oder Provider muss vor dem nächsten Ladeversuch registriert sein. Das Beispiel für getrennte Prozesse nutzt gegenseitiges TLS und eine lokale Entwicklungs-CA. Für den Betrieb stellt der Betreiber Zertifikate und die Zuordnung von Identitäten zu Spielrollen bereit.
- **Nachweis.** `npm run test:game` baut im Checkout ein separates Spiel aus dem aktuellen Git-Commit und prüft zwei Prozessstarts. `cargo test --locked -p zz-mesh -p infinite-stars` prüft die Bibliothek und echte Worker-Prozesse. `npm run bench:clusters` misst getrennte, wechselnde und verschmelzende Spielergruppen. `npm run bench:cluster-wire` misst Sichtbarkeit und Verkehr über TCP/mTLS. [Betriebsanleitung](../../docs/infinite-stars.md) · [Messwerte](../../docs/cluster-performance.md)
