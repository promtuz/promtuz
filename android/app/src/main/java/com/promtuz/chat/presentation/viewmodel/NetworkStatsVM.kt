package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.ConnectionStats
import com.promtuz.chat.domain.model.NetworkStats
import com.promtuz.chat.utils.serialization.AppCbor
import com.promtuz.core.API
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.serialization.decodeFromByteArray
import kotlinx.serialization.descriptors.elementNames

class NetworkStatsVM(
    private val api: API
) : ViewModel() {

    private val _stats = MutableStateFlow<NetworkStats?>(null)
    val stats: StateFlow<NetworkStats?> = _stats.asStateFlow()

    private val _isPolling = MutableStateFlow(false)

    companion object {
        private const val POLL_INTERVAL_MS = 1000L
    }

    init {
        println(ConnectionStats.serializer().descriptor.elementNames.toList())

        startPolling()
    }

    fun startPolling() {
        if (_isPolling.value) return
        _isPolling.value = true

        viewModelScope.launch {
            while (isActive && _isPolling.value) {
                try {
                    val stats = api.getNetworkStats()
                    println("NETWORK STATS : ${stats.toHexString()}")

                    val networkStats = AppCbor.instance.decodeFromByteArray<NetworkStats>(stats)


                    _stats.value = networkStats
                } catch (e: Exception) {
                    // Log error but continue polling
                    e.printStackTrace()
                }

                delay(POLL_INTERVAL_MS)
            }
        }
    }

    fun stopPolling() {
        _isPolling.value = false
    }

    override fun onCleared() {
        super.onCleared()
        stopPolling()
    }
}