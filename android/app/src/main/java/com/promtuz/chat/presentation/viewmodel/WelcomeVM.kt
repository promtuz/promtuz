package com.promtuz.chat.presentation.viewmodel

//import com.promtuz.chat.data.remote.ConnectionError
import android.app.Application
import androidx.compose.runtime.State
import androidx.compose.runtime.mutableStateOf
import androidx.lifecycle.ViewModel
import com.promtuz.chat.presentation.state.WelcomeField
import com.promtuz.chat.presentation.state.WelcomeStatus
import com.promtuz.chat.presentation.state.WelcomeUiState
import com.promtuz.core.API
import org.koin.core.component.KoinComponent

class WelcomeVM(
    private val application: Application,
    private val api: API
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

        if (api.welcome(name)) {
            onSuccess()
        } else {
            onChange(WelcomeField.Status, WelcomeStatus.Normal)
        }
    }
}