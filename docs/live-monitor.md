# Live-Monitor für das Mesh

`npm run observe` startet vier lokale Serverprozesse und 96 bewegte Testspieler. Die Server betreiben je eine Bevy-World, tauschen Replikas über mTLS aus und verwenden für diesen Versuch einen gemeinsamen temporären SQLite-Speicher. Der Monitor öffnet seinen HTTP-Port ausschließlich auf Loopback. [Universumsansicht öffnen](http://127.0.0.1:4174/)

## Was in einem Durchgang passiert

Die Szene wiederholt sich alle 64 Sekunden. Bei 96 Spielern startet jede Insel mit 16 Spielern. Die Farbe eines Punktes zeigt zunächst seinen zuständigen Server. Kontur und Kennung kennzeichnen die räumliche Insel. In der Seitenleiste bedeutet **W1 × 8**, dass W1 acht Spieler dieser Insel simuliert.

| Situation | Auf der Karte und im Protokoll |
| --- | --- |
| Getrennte Inseln | C01 gehört W1, C02 gehört W2. Zwischen diesen beiden Servern besteht anfangs keine Replikationsroute. |
| Zwei Server in derselben Insel | In C03 simulieren W1 und W3 von Beginn an je acht Spieler. Beide besitzen zugleich Spieler auf anderen, räumlich getrennten Inseln. |
| Schrittweise Verschmelzung | C01 und C02 werden eine Insel mit 32 Spielern auf W1 und W2. Danach verschmelzen C05 und C06 zu einer Insel mit 32 Spielern auf W2, W3 und W4. |
| Übergabe der Schreibberechtigung | Im westlichen Zusammenschluss übernimmt W2 den Spieler P091 von W1. Bei der Trennung übernimmt W1 ihn zurück. Der Versuch löst dafür den regulären mTLS-Handoff aus. Im Inspektor ist der neue Server nach der Übergabe sichtbar. |
| Trennung und einzelner Spielerwechsel | Beide Paare bewegen sich wieder auseinander. P094 besucht anschließend C05 und kehrt nach C04 zurück. W4 bleibt für ihn zuständig. |

Der Zeitablauf gibt den Testspielern Wege vor und löst die Serverübergabe gezielt aus. Der räumliche Broker errechnet die tatsächlichen Inseln im 3D-Octree. Das Ereignisprotokoll vergleicht beobachtete Mitgliedschaften und Zuständigkeiten. Eine neue gemeinsame Insel kann entstehen, sobald sich die Aktivitätsbereiche ihrer Randspieler berühren.

## Karte und Messwerte lesen

- Die Karte projiziert X und Z in Metern. Eine Kontur umschließt die projizierten Aktivitätsboxen einer Insel. Die kleinste Spielerkennung ergibt die Clusternummer. Ziehen verschiebt die Karte, Mausrad und Plus/Minus zoomen. Das Fadenkreuz passt die Szene ein.
- Eine Clusterzeile hebt die zugehörigen Spieler hervor. Ein Punkt oder die Spielerliste öffnet den Inspektor mit Server, Position, Empfangsrate und zuletzt dekodiertem Tick. Die Sichtlinien verbinden den ausgewählten Spieler mit den Nachbarn in seinem letzten empfangenen Positionsframe. Die Farbwahl schaltet zwischen Server, Cluster und Client-Hz um.
- **Server-Takt** zeigt den langsamsten der vier Prozesse. Jeder Worker zählt seine eigenen abgeschlossenen Ticks mit einer monotonen Uhr. Die Tabelle enthält die Einzelraten, die Anzahl lokal simulierter Spieler und installierter Replikas. Die Prozess-ID steht im Tooltip der Instanz. Die Tickarbeit umfasst Bewegung, Bevy-Update und den Peer-Schritt.
- **Client-Empfang** zählt vollständig dekodierte Positionsframes je Spieler auf einem eigenen UDP-Socket. Der Testfeed bündelt öffentliche Positionen, die auf diesem Worker vorhanden sind und deren Zentrum innerhalb des Sichtbereichs des Spielers liegt. Positionen anderer Server kommen über die mTLS-Replikation in die lokale World. Jeder vierte Spieler bezieht den Testfeed mit 10 Hz, die übrigen mit 20 Hz. Median und Minimum verwenden rund zwei Sekunden Messzeit.
- Die Replikationsrouten zählen gerichtete Serverpaare mit einer aktiven räumlichen Subscription. Das Protokoll zeigt zusätzlich die Rate tatsächlich angewandter Peer-Updates. Administrative Abfragen laufen höchstens viermal pro Sekunde. Die Kurve hält 60 Sekunden.

Beim Öffnen oder Wiederverbinden lädt der Browser eine Momentaufnahme mit Verlauf und Ereignissen. Anschließend sendet `/api/live` per Server-Sent Events die aktuelle Karte und höchstens einen neuen Kurvenpunkt pro Messung. Ereignisse kommen bei Änderungen mit. Ein langsamer Leser setzt mit einer frischen Momentaufnahme auf. Für eigene Diagnosewerkzeuge liefert `GET /api/state` weiterhin den vollständigen JSON-Zustand. Der Launcher serialisiert ihn erst bei Bedarf.

Im Windows-Test mit 96 bewegten Spielern, vier Workerprozessen und gefülltem Verlauf sank die Nutzlast für einen Zuschauer von **173 auf 67 kB/s**. Gemessen wurde je ein vollständiger 32-Sekunden-Zyklus, zuerst mit vier Vollabfragen pro Sekunde und dann mit dem Live-Stream. Der langsamste Worker erreichte in beiden Läufen im Median **20,0 Hz**. Beim Client-Empfang lagen Median und Minimum bei **20,0 und 10,0 Hz**, passend zu den zwei eingestellten Senderaten. [Messdaten](benchmarks/kernskalierung-10000.json.gz)

Der Observer ist ein eigenes Release-Beispiel. Sim-Server und reguläre Spielworker starten weder die Empfänger noch die Weboberfläche. Der lokale Versuch misst die gezeigte Szene mit UDP-Testclients. Die Lasttests für die Spielprotokolle und 10.000 Verbindungen haben eigene Messpfade. [Testprogramm](../examples/infinite-stars/examples/universe_monitor.rs) · [Empfangsmessung](../scripts/observer/telemetry.mjs) · [Kapazitätsmessungen](tickprofil.md)

## Starten und beenden

Der erste Start baut das Rust-Beispiel. Die Spielerzahl liegt zwischen 12 und 120 und muss durch sechs teilbar sein. Bei anderen Spielerzahlen sind die beiden besonderen Spieler im Auswahlfeld als *Serverwechsel* und *Inselwechsel* markiert.

```sh
npm run observe
npm run observe -- --players 48 --cycle-seconds 30 --duration 120 --port 4176
npm run observe -- --port 4176 --stop
npm run test:observe
```

Die Pausentaste hält die Ansicht an, während der Test weiterläuft. **Test beenden**, Strg+C oder `npm run observe -- --stop` schließen die Worker und Empfänger und räumen die Testwelt auf. Zehn Minuten nach dem letzten Abruf der Admin-Daten beendet sich der Versuch ebenfalls.
