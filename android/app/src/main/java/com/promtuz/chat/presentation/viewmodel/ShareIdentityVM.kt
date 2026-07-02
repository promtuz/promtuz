package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.core.API
import com.promtuz.core.events.IdentityEvent
import com.promtuz.core.events.InternalEvents
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.launch
import kotlinx.serialization.ExperimentalSerializationApi

@OptIn(ExperimentalSerializationApi::class)
class ShareIdentityVM(
//    private val application: Application,
//    private val imgUtils: ImageUtils,
    private val api: API,
) : ViewModel() {
//    private val context: Context get() = application.applicationContext

    private var _qrData = MutableStateFlow<ByteArray?>(null)
    val qrData = _qrData.asStateFlow()

    fun setQR(qr: ByteArray) {
        _qrData.value = qr
    }

    private var _identityRequest = MutableStateFlow<IdentityEvent.AddMe?>(null)
    val identityRequest = _identityRequest.asStateFlow()

    fun rejectRequest() {
        api.identityReject()
        _identityRequest.value = null
    }

    fun acceptRequest() {
        api.identityAccept()
        _identityRequest.value = null
    }

    init {
        viewModelScope.launch {
            val identityEvents = api.eventsFlow.filterIsInstance<InternalEvents.IdentityEv>()
                .distinctUntilChanged()

            identityEvents.collect { ev ->
                when (ev) {
                    is IdentityEvent.AddMe -> {
                        _identityRequest.value = ev
                    }
                }
            }
        }
    }
}