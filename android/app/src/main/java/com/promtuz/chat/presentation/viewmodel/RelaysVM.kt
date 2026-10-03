package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import timber.log.Timber
import uniffi.core.RelayCircuit
import uniffi.core.RelayStat

enum class RelayStatus { LIVE, IDLE, PROBING, DOWN }

data class UiRelay(
    val id: String,
    val host: String,
    val port: Int,
    val status: RelayStatus,
    val lastLatencyMs: Long?,
    val windowAttempts: Int,
    val windowSuccesses: Int,
    val consecutiveFailures: Int,
    val lastConnectMs: Long?,
    val backoffUntilMs: Long?,
    val isConnected: Boolean,
    /** RTT in ms, oldest first. */
    val latencySamples: List<Float>,
) {
    val canReset get() = status == RelayStatus.PROBING || status == RelayStatus.DOWN
}

/** Polls, since core has no relay event stream; the VM is screen-scoped, so polling stops with the page. */
class RelaysVM : ViewModel() {
    private val _relays = MutableStateFlow<List<UiRelay>>(emptyList())
    val relays: StateFlow<List<UiRelay>> = _relays.asStateFlow()

    init {
        viewModelScope.launch {
            while (isActive) {
                refresh()
                delay(POLL_MS)
            }
        }
    }

    private suspend fun refresh() {
        try {
            _relays.value = CoreBridge.relays()
                .map { it.toUi() }
                .sortedWith(compareByDescending<UiRelay> { it.isConnected }.thenByDescending { it.lastConnectMs ?: 0L })
        } catch (e: Exception) {
            Timber.tag("RelaysVM").e(e, "Failed to load relays")
        }
    }

    fun resetCircuit(id: String) = act { CoreBridge.resetRelayCircuit(id) }
    fun forget(id: String) = act { CoreBridge.forgetRelay(id) }
    fun connect(id: String) = act { CoreBridge.connectRelay(id) }

    private inline fun act(crossinline block: suspend () -> Unit) {
        viewModelScope.launch {
            try {
                block()
            } catch (e: Exception) {
                Timber.tag("RelaysVM").e(e, "Relay action failed")
            }
            refresh()
        }
    }

    private fun RelayStat.toUi(): UiRelay {
        val status = when {
            isConnected -> RelayStatus.LIVE
            circuitState == RelayCircuit.OPEN -> RelayStatus.DOWN
            circuitState == RelayCircuit.HALF_OPEN -> RelayStatus.PROBING
            else -> RelayStatus.IDLE
        }
        return UiRelay(
            id = id,
            host = host,
            port = port.toInt(),
            status = status,
            lastLatencyMs = lastLatency?.toLong(),
            windowAttempts = windowAttempts.toInt(),
            windowSuccesses = windowSuccesses.toInt(),
            consecutiveFailures = consecutiveFailures.toInt(),
            lastConnectMs = lastConnect?.toLong(),
            backoffUntilMs = backoffUntil?.toLong(),
            isConnected = isConnected,
            latencySamples = latencySamples.map { it.toFloat() },
        )
    }

    companion object {
        private const val POLL_MS = 1500L
    }
}
