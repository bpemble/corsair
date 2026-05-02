#!/bin/bash
# Wrapper around the stock gnzsnz/ib-gateway entrypoint that also launches
# the TOTP helper in the background. The helper watches for the 2FA dialog
# and auto-enters the code via xdotool.

# The stock run.sh sets DISPLAY=:1 internally, but our helper forks before
# that happens. Export it here so xdotool can find the gateway's X server.
export DISPLAY=:1

# ── JVM tuning for lower amend-RTT tails ──────────────────────────────
# Gateway ships with -Xmx768m and -XX:MaxGCPauseMillis=200, which both
# invite GC-induced tail spikes. Py-spy profile on 2026-04-19 confirmed
# our Python side is 95%+ idle, so the ~300ms overhead under load is
# almost entirely in the Gateway's Java OMS. Patch the vmoptions file
# before handing off to IBC.
#  - Fixed 2 GB heap (no resize pauses, more slack for G1)
#  - MaxGCPauseMillis: 200 → 20 (10× tighter pause target)
#  - AlwaysPreTouch: commit all heap pages at startup (avoid first-
#    touch microstalls on live trading path)
# Safe to run every boot: sed patterns are idempotent (no-op after the
# first run). Path uses glob so a Gateway auto-update to 10.46.x etc
# still matches without edits here.
_vmopts=$(ls /home/ibgateway/Jts/ibgateway/*/ibgateway.vmoptions 2>/dev/null | head -1)
if [ -n "$_vmopts" ]; then
    # Heap pin: from default 768m → 4g, or bump prior 2g → 4g.
    # 2026-05-02 (Alabaster post-bare-metal): bumped 2g→4g while we
    # had the bonnet open. We have 31GB RAM and the gateway is the
    # only JVM; less GC frequency is free.
    if grep -q '^-Xmx768m$' "$_vmopts"; then
        sed -i 's/^-Xmx768m$/-Xms4g\n-Xmx4g/' "$_vmopts"
        echo ".> JVM heap pinned to 4g (Xms=Xmx)"
    elif grep -q '^-Xms2g$' "$_vmopts" && ! grep -q '^-Xms4g$' "$_vmopts"; then
        sed -i 's/^-Xms2g$/-Xms4g/; s/^-Xmx2g$/-Xmx4g/' "$_vmopts"
        echo ".> JVM heap bumped 2g → 4g"
    fi
    if grep -q '^-XX:MaxGCPauseMillis=200$' "$_vmopts"; then
        sed -i 's/^-XX:MaxGCPauseMillis=200$/-XX:MaxGCPauseMillis=20/' "$_vmopts"
        echo ".> G1GC MaxGCPauseMillis tightened 200 → 20ms"
    fi
    if ! grep -q 'AlwaysPreTouch' "$_vmopts"; then
        echo '-XX:+AlwaysPreTouch' >> "$_vmopts"
        echo ".> JVM AlwaysPreTouch enabled"
    fi
    # 2026-05-02 latency experiment: G1 → ZGC. ZGC targets sub-ms
    # GC pauses (G1 targets 20ms here). Helps p99/p99.9 of place_rtt_us
    # when GC happens to fire during an order. Java 17 production ZGC.
    # Revert: change UseZGC back to UseG1GC.
    if grep -q '^-XX:+UseG1GC$' "$_vmopts"; then
        sed -i 's/^-XX:+UseG1GC$/-XX:+UseZGC/' "$_vmopts"
        echo ".> JVM GC: G1 → ZGC (latency experiment 2026-05-02)"
    fi
    # 2026-05-02 latency experiment: ParallelGCThreads 20 → 4. Default
    # of 20 was sized for big servers; gateway runs in cpuset 0,1,6,7
    # (4 logical cores), so 20 STW threads contend with itself. ZGC
    # ignores this opt — harmless once UseZGC is on.
    # Revert: change 4 back to 20.
    if grep -q '^-XX:ParallelGCThreads=20$' "$_vmopts"; then
        sed -i 's/^-XX:ParallelGCThreads=20$/-XX:ParallelGCThreads=4/' "$_vmopts"
        echo ".> JVM ParallelGCThreads: 20 → 4 (matches cpuset)"
    fi
fi

# Launch TOTP helper if secret is configured
if [ -f /mnt/scripts/totp_helper.sh ] && [ -n "$TOTP_SECRET" ]; then
    echo ".> Launching TOTP helper in background"
    bash /mnt/scripts/totp_helper.sh &
fi

# Hand off to the stock entrypoint
exec /home/ibgateway/scripts/run.sh
