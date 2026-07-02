//
// Created by bhuvnesh on 31/08/25.
//

#ifndef PROMTUZ_CORE_BINDINGS_H
#define PROMTUZ_CORE_BINDINGS_H

#include <cstdint>

#ifdef __cplusplus
extern "C" {
#endif

extern int c_get_static_key(const uint8_t *sk_ptr);

#ifdef __cplusplus
}
#endif

#endif //PROMTUZ_CORE_BINDINGS_H
