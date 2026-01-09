#!/usr/bin/with-contenv bashio

# Build command line arguments
ARGS=()

# MQTT configuration - prefer explicit config, fall back to service discovery
if bashio::config.has_value 'mqtt_host'; then
    ARGS+=(--mqtt-host "$(bashio::config 'mqtt_host')")
else
    ARGS+=(--mqtt-host "$(bashio::services mqtt 'host')")
fi

if bashio::config.has_value 'mqtt_username'; then
    ARGS+=(--mqtt-username "$(bashio::config 'mqtt_username')")
else
    ARGS+=(--mqtt-username "$(bashio::services mqtt 'username')")
fi

if bashio::config.has_value 'mqtt_password'; then
    ARGS+=(--mqtt-password "$(bashio::config 'mqtt_password')")
else
    ARGS+=(--mqtt-password "$(bashio::services mqtt 'password')")
fi

# ebusd address - required, but we can try to discover it from the supervisor API
if bashio::config.has_value 'ebusd_address'; then
    ARGS+=(--ebusd-address "$(bashio::config 'ebusd_address')")
else
    # Try to find ebusd add-on via supervisor API
    EBUSD_ADDON=$(bashio::api.supervisor GET "/addons" false '.addons[] | select(.slug | endswith("_ebusd") or . == "local_ebusd") | .slug' 2>/dev/null | head -1)
    if [[ -n "${EBUSD_ADDON}" ]]; then
        # Convert slug to hostname (replace _ with -)
        EBUSD_HOST="${EBUSD_ADDON//_/-}"
        ARGS+=(--ebusd-address "${EBUSD_HOST}:8888")
        bashio::log.info "Auto-discovered ebusd at: ${EBUSD_HOST}:8888"
    else
        bashio::log.fatal "No ebusd_address configured and could not auto-discover ebusd add-on"
        bashio::log.fatal "Please set ebusd_address in the add-on configuration"
        bashio::exit.nok
    fi
fi

if bashio::config.has_value 'thermometer_entity'; then
    ARGS+=(--thermometer-entity "$(bashio::config 'thermometer_entity')")
fi

if bashio::config.has_value 'tap_water_temp'; then
    ARGS+=(--tap-water-temp "$(bashio::config 'tap_water_temp')")
fi

if bashio::config.has_value 'temperature_band'; then
    ARGS+=(--temperature-band "$(bashio::config 'temperature_band')")
fi

if bashio::config.has_value 'ha_api_address'; then
    ARGS+=(--ha-api-address "$(bashio::config 'ha_api_address')")
fi

if bashio::config.has_value 'ha_api_token'; then
    ARGS+=(--ha-api-token "$(bashio::config 'ha_api_token')")
fi

bashio::log.info "Starting ebus-thermostat with args: ${ARGS[*]}"

exec /usr/local/bin/ebus-thermostat "${ARGS[@]}"
