package com.promtuz.chat.presentation.viewmodel

//import com.promtuz.chat.data.remote.ConnectionError
import android.app.Application
import androidx.compose.runtime.State
import androidx.compose.runtime.mutableStateOf
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.presentation.state.WelcomeField
import com.promtuz.chat.presentation.state.WelcomeStatus
import com.promtuz.chat.presentation.state.WelcomeUiState
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.launch
import org.koin.core.component.KoinComponent
import uniffi.core.CoreException

class WelcomeVM(
    private val application: Application
) : ViewModel(), KoinComponent {

    private val _uiState = mutableStateOf(
        WelcomeUiState(
            "", WelcomeStatus.Normal, null
        )
    )
    val uiState: State<WelcomeUiState> = _uiState

    fun <T> onChange(field: WelcomeField, value: T) {
        _uiState.value = when (field) {
            WelcomeField.Name -> _uiState.value.copy(name = value as String)
            WelcomeField.Error -> _uiState.value.copy(errorText = value as String?)
            WelcomeField.Status -> _uiState.value.copy(status = value as WelcomeStatus)
        }
    }

    fun `continue`(onSuccess: () -> Unit) {
        val name = uiState.value.name
        if (name.isEmpty()) return

        onChange(WelcomeField.Status, WelcomeStatus.Generating)

        viewModelScope.launch {
            try {
                CoreBridge.enroll(name)
                // Channel A escrow — best-effort; Channel B (phrase) always exists.
                RecoveryStore.escrow(application)
                onSuccess()
            } catch (e: CoreException) {
                onChange(WelcomeField.Status, WelcomeStatus.Normal)
            }
        }
    }
}
