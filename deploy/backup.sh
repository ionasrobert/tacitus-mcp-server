#!/bin/sh
# Snapshot al stării unui relay: logurile CRDT cifrate ale vault-urilor +
# entitlements.json. Rulează-l din cron, zilnic:
#
#   25 3 * * * flock -n /tmp/tacitus-backup.lock /path/to/backup.sh >> /var/log/tacitus-backup.log 2>&1
#
# De ce merită, deși datele sunt de ordinul zecilor de KB: nimic din ele nu se
# reconstruiește complet. entitlements.json se poate reface din Stripe
# (`tacitus-billing --reconcile`), dar DOAR pentru abonamentele plătite — o
# intrare comp, fără `stripe_subscription`, n-ar mai exista nicăieri. Iar logul
# e cifrat cu o cheie pe care serverul n-o are: dacă îl pierzi, nu-l poate
# regenera nimeni de aici.
#
# Arhiva se scrie sub `.part` și se redenumește la final, ca o rulare
# întreruptă să nu lase în urmă un tar trunchiat care arată ca un backup bun.
# Copia se ia la cald: relay-ul scrie append-only în log.jsonl și redenumește
# atomic snapshot-urile, deci în cel mai rău caz ultima linie e tăiată.
set -eu

SRC=${TACITUS_RELAY_DATA:-/var/lib/tacitus-relay}
DST=${TACITUS_BACKUP_DIR:-/var/backups/tacitus}
KEEP=${TACITUS_BACKUP_KEEP:-14}

mkdir -p "$DST"
stamp=$(date -u +%Y%m%d-%H%M)
part="$DST/.tacitus-relay-$stamp.tar.gz.part"
final="$DST/tacitus-relay-$stamp.tar.gz"

tar -czf "$part" -C "$(dirname "$SRC")" "$(basename "$SRC")"
gzip -t "$part"
mv "$part" "$final"

# Retenție: cele mai noi $KEEP arhive rămân, restul pleacă.
ls -1t "$DST"/tacitus-relay-*.tar.gz 2>/dev/null | tail -n +$((KEEP + 1)) | while read -r old; do
	rm -f -- "$old"
done

printf '%s ok %s (%s)\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$final" "$(du -h "$final" | cut -f1)"
