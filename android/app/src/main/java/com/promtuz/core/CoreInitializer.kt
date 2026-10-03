package com.promtuz.core

import android.content.Context
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.security.KeyManager
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import timber.log.Timber
import uniffi.core.init as ffiInit

/** Core's init throws when called twice, hence the guard. Empty resolver seeds leave core disconnected. */
object CoreInitializer {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    @Volatile
    private var started = false

    @Synchronized
    fun start(context: Context) {
        if (started) return
        started = true
        val app = context.applicationContext
        scope.launch {
            try {
                ffiInit(KeyManager, CoreEventBus, BuildConfig.RESOLVER_SEEDS)
                CoreNetworkMonitor.start(app)
            } catch (e: Exception) {
                Timber.tag("CoreInitializer").e(e, "libcore init failed")
            }
        }
    }
}
