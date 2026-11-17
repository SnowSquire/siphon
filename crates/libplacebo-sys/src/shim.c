// `pl_log_create` is a macro: `pl_log_create(api_ver, params)` expands to
// `pl_log_create_<api_ver>(params)`, and the number is the *library's* API
// version, not ours. bindgen cannot translate that macro, which is why the
// Windows build ended up calling `pl_log_create_372` directly and broke against
// any other libplacebo. Calling it here, with the header's own PL_API_VER, gives
// the crate one symbol that links against whatever version is installed.
#include <libplacebo/log.h>

pl_log siphon_pl_log_create(const struct pl_log_params *params)
{
    return pl_log_create(PL_API_VER, params);
}
