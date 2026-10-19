/* Prints the size of every struct in emusync.h and the offset of every field, as this C
   compiler lays them out. tests/c_abi.rs compares the output with the Rust definitions. */
#include "emusync.h"

#include <stddef.h>
#include <stdio.h>

#define S(T) printf("%s %u\n", #T, (unsigned)sizeof(T))
#define F(T, f) printf("%s.%s %u\n", #T, #f, (unsigned)offsetof(T, f))

int main(void) {
    S(es_status);
    S(es_log_level);
    S(es_event_type);
    S(es_card_mode);

    S(es_config);
    F(es_config, data_dir);
    F(es_config, device_name);
    F(es_config, emulator_id);
    F(es_config, emulator_version);
    F(es_config, vfs);
    F(es_config, conflict_policy);
    F(es_config, flags);
    F(es_config, snapshot_budget_bytes);
    F(es_config, log);
    F(es_config, log_user);
    F(es_config, log_level);

    S(es_credentials);
    F(es_credentials, kind);
    F(es_credentials, access_key_id);
    F(es_credentials, secret_access_key);
    F(es_credentials, session_token);
    F(es_credentials, expires_unix_ms);
    F(es_credentials, bearer_token);

    S(es_credential_provider);
    F(es_credential_provider, get);

    S(es_container_desc);
    F(es_container_desc, format);
    F(es_container_desc, location);
    F(es_container_desc, platform);
    F(es_container_desc, card);
    F(es_container_desc, title_scope);
    F(es_container_desc, flags);

    S(es_title);
    F(es_title, platform);
    F(es_title, serial);
    F(es_title, content_hash);
    F(es_title, display_name);

    S(es_session_begin_args);
    F(es_session_begin_args, title);
    F(es_session_begin_args, mounted);
    F(es_session_begin_args, mounted_count);
    F(es_session_begin_args, timeout_ms);
    F(es_session_begin_args, flags);
    F(es_session_begin_args, state_compat_key);

    S(es_session_info);
    F(es_session_info, id);
    F(es_session_info, units_applied);
    F(es_session_info, units_blocked);
    F(es_session_info, conflicts);
    F(es_session_info, remote_reached);
    F(es_session_info, other_device_active);
    F(es_session_info, resume_state_id);
    F(es_session_info, resume_state_unix_ms);

    S(es_state_desc);
    F(es_state_desc, title);
    F(es_state_desc, path);
    F(es_state_desc, compat_key);
    F(es_state_desc, slot);
    F(es_state_desc, screenshot_path);

    S(es_state_info);
    F(es_state_info, id);
    F(es_state_info, device_name);
    F(es_state_info, created_unix_ms);
    F(es_state_info, size);
    F(es_state_info, stale_vs_saves);

    S(es_version_info);
    F(es_version_info, version_id);
    F(es_version_info, device_name);
    F(es_version_info, modified_unix_ms);
    F(es_version_info, playtime_ms);
    F(es_version_info, size);
    F(es_version_info, is_tombstone);

    S(es_conflict_info);
    F(es_conflict_info, container);
    F(es_conflict_info, unit_id);
    F(es_conflict_info, unit_name);
    F(es_conflict_info, unit_title);
    F(es_conflict_info, title_key);
    F(es_conflict_info, local);
    F(es_conflict_info, remote);
    F(es_conflict_info, remote_count);

    S(es_event);
    F(es_event, type);
    F(es_event, status);
    F(es_event, container);
    F(es_event, conflict_id);
    F(es_event, bytes_done);
    F(es_event, bytes_total);
    F(es_event, title_key);
    F(es_event, unit_id);
    F(es_event, unit_name);
    F(es_event, device_name);
    F(es_event, message);
    F(es_event, _internal);
    return 0;
}
