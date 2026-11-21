#!/usr/bin/env bash
# Capture one Twitch broadcast in both orientations, live and DVR: four
# N_m3u8DL-RE processes.
#
#   scripts/capture-twitch.sh -c snyozeniken [--out DIR] [--dry-run]
#
# Why four: the live playlist carries the ~2 s segments as published (with the
# ad markers and the discontinuities), the DVR/archive playlist carries the
# whole broadcast in ~10 s chunks from segment 0. Capturing both is the only way
# to compare what Twitch actually serves against what our recorder assumes
# (notes/disk-complete-recording.md), and doing it per orientation is how we
# find out whether landscape is HEVC and portrait H.264 and whether both are
# fMP4.
#
# The variant split is by RESOLUTION, not by name: Twitch's STABLE-VARIANT-ID
# for a portrait track is not reliably "*-portrait" (a portrait-only channel
# emits "1920p" at 1080x1920), so width vs height is the only dependable signal.
#
# Needs: curl, jq, sha1sum, xxd, shuf, N_m3u8DL-RE on PATH.
set -euo pipefail

shopt -s inherit_errexit || {
	echo "$0: needs bash 4.4+ (shopt inherit_errexit)" >&2
	exit 1
}

CHANNEL=snyozeniken
OUT=""
DRY_RUN=0
CLIENT_ID=${TWITCH_CLIENT_ID:-kimne78kx3ncx6brgo4mv6wki5h1ko}
GQL=https://gql.twitch.tv/gql
USHER=https://usher.ttvnw.net/api/v2/channel/hls
VOD_DOMAINS=(
	https://ds0h3roq6wcgc.cloudfront.net
	https://d2nvs31859zcd8.cloudfront.net
	https://d2aba1wr3818hz.cloudfront.net
	https://d3c27h4odz752x.cloudfront.net
	https://dgeft87wbj63p.cloudfront.net
	https://d1m7jfoe9zdc1j.cloudfront.net
	https://d3vd9lfkzbru3h.cloudfront.net
	https://ddacn6pr5v0tl.cloudfront.net
	https://d3aqoihi2n8ty8.cloudfront.net
	https://d3fi1amfgojobc.cloudfront.net
	https://d2vi6trrdongqn.cloudfront.net
	https://d3stzm2eumvgb4.cloudfront.net
)

while [ $# -gt 0 ]; do
	case "$1" in
	-c | --channel)
		CHANNEL=$2
		shift 2
		;;
	-o | --out)
		OUT=$2
		shift 2
		;;
	-n | --dry-run)
		DRY_RUN=1
		shift
		;;
	*)
		echo "usage: $0 [-c channel] [-o dir] [--dry-run]" >&2
		exit 2
		;;
	esac
done

OUT=${OUT:-captures/$CHANNEL-$(date -u +%Y%m%d-%H%M%S)}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(dirname "$here")

# ---------------------------------------------------------------- playback token

playback_token() {
	local body sig value
	body='{"operationName":"PlaybackAccessToken","extensions":{"persistedQuery":{"sha256Hash":"ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9","version":1}},"variables":{"isLive":true,"isVod":false,"login":"'"$CHANNEL"'","playerType":"site","platform":"site","vodID":""}}'
	local response
	response=$(curl -sS -m 30 -X POST "$GQL" \
		-H "Client-ID: $CLIENT_ID" \
		-H 'Content-Type: text/plain;charset=UTF-8' \
		--data-binary "$body")
	sig=$(jq -r '.data.streamPlaybackAccessToken.signature // empty' <<<"$response")
	value=$(jq -r '.data.streamPlaybackAccessToken.value // empty' <<<"$response" | tr -d '\n')
	[ -n "$sig" ] && [ -n "$value" ] || {
		echo "capture: no playback token for $CHANNEL (offline, or not a live channel)" >&2
		return 1
	}
	echo "$sig"
	echo "$value"
}

# ------------------------------------------------------------------- live master

live_master() {
	local sig value session p
	{ read -r sig; read -r value; } < <(playback_token)
	session=$(head -c16 /dev/urandom | xxd -p)
	p=$(shuf -i 0-9999999 -n1)
	# The parameter list is the one in crates/daemon/src/variant.rs; in
	# particular multigroup_video=true is what makes Twitch list the vertical
	# variants at all.
	curl -sS -m 30 --get "$USHER/$CHANNEL.m3u8" \
		--data-urlencode "sig=$sig" \
		--data-urlencode "token=$value" \
		--data-urlencode 'allow_source=true' \
		--data-urlencode 'allow_audio_only=true' \
		--data-urlencode 'allow_spectre=true' \
		--data-urlencode 'browser_family=chrome' \
		--data-urlencode 'browser_version=151.0' \
		--data-urlencode 'cdm=wv' \
		--data-urlencode 'enable_score=true' \
		--data-urlencode 'fast_bread=false' \
		--data-urlencode 'include_unavailable=true' \
		--data-urlencode 'lang=en' \
		--data-urlencode 'os_name=Windows' \
		--data-urlencode 'os_version=NT 10.0' \
		--data-urlencode "p=$p" \
		--data-urlencode 'platform=web' \
		--data-urlencode "play_session_id=$session" \
		--data-urlencode 'player_backend=mediaplayer' \
		--data-urlencode 'playlist_include_framerate=true' \
		--data-urlencode 'reassignments_supported=true' \
		--data-urlencode 'supported_codecs=av1,h265,h264' \
		--data-urlencode 'transcode_mode=cbr_v1' \
		--data-urlencode 'multigroup_video=true'
}

# `kind bandwidth id resolution url`, one line per video variant.
# kind: portrait | landscape | audio, from RESOLUTION (falling back to the id).
variant_rows() {
	awk '/^#EXT-X-STREAM-INF:/ { info = $0; next } /^https?:/ { if (info != "") { print info "\t" $0; info = "" } }' |
		while IFS=$'\t' read -r info url; do
			local w h bw id kind
			w=$(sed -nE 's/.*RESOLUTION=([0-9]+)x([0-9]+).*/\1/p' <<<"$info")
			h=$(sed -nE 's/.*RESOLUTION=([0-9]+)x([0-9]+).*/\2/p' <<<"$info")
			bw=$(sed -nE 's/.*BANDWIDTH=([0-9]+).*/\1/p' <<<"$info")
			id=$(sed -nE 's/.*STABLE-VARIANT-ID="([^"]+)".*/\1/p' <<<"$info")
			if [ -z "$w" ] || [ -z "$h" ]; then
				kind=audio
			elif [ "$h" -gt "$w" ]; then
				kind=portrait
			else
				kind=landscape
			fi
			printf '%s %s %s %sx%s %s\n' "$kind" "${bw:-0}" "${id:-?}" "${w:-?}" "${h:-?}" "$url"
		done | sort -k1,1 -k2,2nr
}

# The highest-bandwidth variant of one orientation.
pick() {
	local kind=$1 rows=$2
	awk -v k="$kind" '$1 == k { print $5; exit }' <<<"$rows"
}

# ------------------------------------------------------------- private DVR paths

# `sha1("{channel}_{stream_id}_{start}")[..20]_{path}` is Twitch's private DVR
# object name (crates/daemon/src/rtsp/dvr.rs). `probe_dvr <path>` tries the
# start-time offsets Twitch uses, on every edge host.
probe_dvr() {
	local path=$1 stream_id=$2 start=$3 offset name hash domain url
	for offset in 0 -1 1; do
		name="${CHANNEL}_${stream_id}_$((start + offset))"
		hash=$(printf '%s' "$name" | sha1sum | cut -c1-20)
		for domain in "${VOD_DOMAINS[@]}"; do
			url="$domain/${hash}_${name}/$path"
			if [ "$(curl -s -o /dev/null -m 8 -w '%{http_code}' -I "$url")" = 200 ]; then
				echo "$url"
				return 0
			fi
		done
	done
	return 1
}

stream_meta() {
	local response
	response=$(curl -sS -m 30 -X POST "$GQL" \
		-H "Client-ID: $CLIENT_ID" \
		-H 'Content-Type: text/plain;charset=UTF-8' \
		--data-binary '[{"operationName":"StreamMetadata","variables":{"channelLogin":"'"$CHANNEL"'","includeIsDJ":false},"extensions":{"persistedQuery":{"version":1,"sha256Hash":"b57f9b910f8cd1a4659d894fe7550ccc81ec9052c01e438b290fd66a040b9b93"}}}]')
	local user id created
	user=$(jq -r '.[0].data.user.id // empty' <<<"$response")
	id=$(jq -r '.[0].data.user.stream.id // empty' <<<"$response")
	created=$(jq -r '.[0].data.user.stream.createdAt // empty' <<<"$response")
	[ -n "$id" ] && [ -n "$created" ] || {
		echo "capture: no stream metadata for $CHANNEL" >&2
		return 1
	}
	echo "$user"
	echo "$id"
	date -u -d "$created" +%s
}

# The archive master, from the public VOD endpoint. While a broadcast is live it
# is published as an `ARCHIVE` video, so this works during the stream: Twitch
# listed a live portrait broadcast as recent VOD 2935138939 with STREAM-TIME at
# 26 h. The master is the authoritative list of which DVR media playlists exist
# (`chunked/` is the source group, `audio_only/` the audio, and a dual-source
# broadcast lists both orientations there with their RESOLUTION).
archive_master() {
	local user_id=$1 start=$3 broadcast response
	broadcast=$(curl -sS -m 30 -X POST "$GQL" \
		-H "Client-ID: $CLIENT_ID" \
		-H 'Content-Type: text/plain;charset=UTF-8' \
		--data-binary '[{"operationName":"FFZ_RecentBroadcasts","variables":{"id":"'"$user_id"'","type":"ARCHIVE","sort":"TIME","limit":1},"extensions":{"persistedQuery":{"version":1,"sha256Hash":"a92c0ad34fdba9b3f5e6125dce4b0acc9373f2b2c06c74c92187749d12f73d4c"}}}]' |
		jq -c '.[0].data.user.videos.edges[0].node // empty')
	[ -n "$broadcast" ] || return 1
	local vod created_ms sig value
	vod=$(jq -r '.id' <<<"$broadcast")
	created_ms=$(date -u -d "$(jq -r '.createdAt' <<<"$broadcast")" +%s)
	# A VOD older than the broadcast is a previous stream, not this archive.
	[ "$created_ms" -ge "$start" ] || return 1

	response=$(curl -sS -m 30 -X POST "$GQL" \
		-H "Client-ID: $CLIENT_ID" \
		-H 'Content-Type: text/plain;charset=UTF-8' \
		--data-binary '{"operationName":"PlaybackAccessToken","extensions":{"persistedQuery":{"sha256Hash":"ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9","version":1}},"variables":{"isLive":false,"isVod":true,"login":"","vodID":"'"$vod"'","playerType":"site","platform":"web"}}')
	sig=$(jq -r '.data.videoPlaybackAccessToken.signature // empty' <<<"$response")
	value=$(jq -r '.data.videoPlaybackAccessToken.value // empty' <<<"$response" | tr -d '\n')
	[ -n "$sig" ] && [ -n "$value" ] || return 1

	curl -sS -m 30 --get "https://usher.ttvnw.net/vod/v2/$vod.m3u8" \
		--data-urlencode "sig=$sig" \
		--data-urlencode "token=$value" \
		--data-urlencode 'allow_source=true' \
		--data-urlencode 'browser_family=chrome' \
		--data-urlencode 'browser_version=157.0' \
		--data-urlencode 'cdm=wv' \
		--data-urlencode 'enable_score=true' \
		--data-urlencode 'include_unavailable=true' \
		--data-urlencode 'lang=en' \
		--data-urlencode 'os_name=Windows' \
		--data-urlencode 'os_version=NT 10.0' \
		--data-urlencode 'platform=web' \
		--data-urlencode 'player_backend=mediaplayer' \
		--data-urlencode 'playlist_include_framerate=true' \
		--data-urlencode 'reassignments_supported=true' \
		--data-urlencode 'supported_codecs=av1,h265,h264' \
		--data-urlencode 'transcode_mode=cbr_v1'
}

# ------------------------------------------------------------------- one capture

declare -a PIDS=()
declare -a LABELS=()

spawn() {
	local label=$1 url=$2
	local dir="$OUT/$label"
	shift 2
	if [ "$DRY_RUN" = 1 ]; then
		printf 'would run: %s -> %s\n  %s\n' "$label" "$dir" "$url"
		return 0
	fi
	mkdir -p "$dir"
	# --skip-merge keeps the segments; --del-after-done false keeps them after
	# the tool exits. The live captures start at the edge (default take count),
	# the DVR captures start at segment 0.
	N_m3u8DL-RE "$url" \
		--save-dir "$dir" --save-name "$label" --tmp-dir "$dir/tmp" \
		--skip-merge --del-after-done false --live-keep-segments true \
		--thread-count 4 --check-segments-count false \
		--no-ansi-color --disable-update-check --log-level INFO \
		"$@" >"$dir/$label.log" 2>&1 &
	PIDS+=($!)
	LABELS+=("$label")
	echo "capture: $label -> $dir (pid $!)"
}

cleanup() {
	if [ ${#PIDS[@]} -gt 0 ]; then
		echo "capture: stopping ${#PIDS[@]} recorder(s)"
		kill "${PIDS[@]}" 2>/dev/null || true
		wait 2>/dev/null || true
	fi
}
trap cleanup INT TERM EXIT

# ------------------------------------------------------------------------ plan

master=$(live_master)
if ! grep -q '^#EXT-X-STREAM-INF' <<<"$master"; then
	echo "capture: $CHANNEL is not live (no variants in the master)" >&2
	exit 1
fi

rows=$(variant_rows <<<"$master")
echo "capture: $CHANNEL variants (kind, bandwidth, id, resolution):"
awk '{ printf "  %-9s %10s  %-12s %s\n", $1, $2, $3, $4 }' <<<"$rows"

live_land=$(pick landscape "$rows")
live_port=$(pick portrait "$rows")
urls_live=()
[ -n "$live_land" ] && urls_live[0]=$live_land
[ -n "$live_port" ] && urls_live[1]=$live_port

mapfile -t meta < <(stream_meta)
user_id=${meta[0]}
stream_id=${meta[1]}
start=${meta[2]}
echo "capture: broadcast $stream_id started $(date -u -d "@$start" +%Y-%m-%dT%H:%M:%SZ)"

# The DVR side, in three steps.
#
# 1. The private DVR has one directory per variant id, so probe the id the live
#    master gave us. This is the only way to reach the portrait group on a
#    dual-format broadcast: the archive master hides it (see step 2).
# 2. The archive master is the only listing a DVR directory has (the root 403s),
#    and it is authoritative for ids that get renamed (live 1920p -> archive
#    1080p-portrait) and for the landscape source, which lives under chunked/
#    rather than under its own id.
# 3. chunked/ as a last resort, when neither found anything. It is the source
#    group and names no orientation, so only a single-orientation stream can use
#    it unambiguously.
dvr_land=""
dvr_port=""
land_id=$(awk '$1 == "landscape" { print $3; exit }' <<<"$rows")
port_id=$(awk '$1 == "portrait" { print $3; exit }' <<<"$rows")
[ -n "$land_id" ] && dvr_land=$(probe_dvr "$land_id/index-dvr.m3u8" "$stream_id" "$start" || true)
[ -n "$port_id" ] && dvr_port=$(probe_dvr "$port_id/index-dvr.m3u8" "$stream_id" "$start" || true)

if [ -z "$dvr_land" ] || [ -z "$dvr_port" ]; then
	archive=$(archive_master "$user_id" "$stream_id" "$start" || true)
	if grep -q '^#EXT-X-STREAM-INF' <<<"$archive"; then
		arows=$(variant_rows <<<"$archive")
		echo "capture: archive variants (kind, bandwidth, id, resolution):"
		awk '{ printf "  %-9s %10s  %-14s %s\n", $1, $2, $3, $4 }' <<<"$arows"
		[ -z "$dvr_land" ] && dvr_land=$(pick landscape "$arows")
		[ -z "$dvr_port" ] && dvr_port=$(pick portrait "$arows")
	else
		echo "capture: no archive master (nothing published at the VOD endpoint)" >&2
	fi
fi

if [ -z "$dvr_land" ] || [ -z "$dvr_port" ]; then
	chunked=$(probe_dvr chunked/index-dvr.m3u8 "$stream_id" "$start" || true)
	if [ -n "$chunked" ]; then
		if [ -n "$live_land" ] && [ -z "$live_port" ]; then
			dvr_land=$chunked
		elif [ -n "$live_port" ] && [ -z "$live_land" ]; then
			dvr_port=$chunked
		elif [ -n "$live_land" ] && [ -n "$live_port" ]; then
			echo "capture: both orientations are live but only chunked/ answers;" >&2
			echo "capture: it is the source group and cannot be attributed, skipping it" >&2
		fi
	fi
fi

echo "capture: DVR landscape ${dvr_land:-<none found>}"
echo "capture: DVR portrait  ${dvr_port:-<none found>}"
[ -n "$OUT" ] && echo "capture: output $OUT"

# ----------------------------------------------------------------------- spawn

[ -n "$live_land" ] && spawn "$CHANNEL-landscape-live" "$live_land" || echo "capture: no landscape variant, skipping live landscape" >&2
[ -n "$live_port" ] && spawn "$CHANNEL-portrait-live" "$live_port" || echo "capture: no portrait variant, skipping live portrait" >&2
[ -n "$dvr_land" ] && spawn "$CHANNEL-landscape-dvr" "$dvr_land" --live-take-count 1000000 ||
	echo "capture: no landscape DVR playlist, skipping" >&2
[ -n "$dvr_port" ] && spawn "$CHANNEL-portrait-dvr" "$dvr_port" --live-take-count 1000000 ||
	echo "capture: no portrait DVR playlist, skipping" >&2

if [ "$DRY_RUN" = 1 ]; then
	echo "capture: dry run, nothing started"
	exit 0
fi

if [ ${#PIDS[@]} -eq 0 ]; then
	echo "capture: nothing to record" >&2
	exit 1
fi

echo "capture: ${#PIDS[@]} recorder(s) running; Ctrl-C stops them"
wait
