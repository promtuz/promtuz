package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.lifecycle.ViewModel

class SettingsVM(
    private val application: Application
) : ViewModel() {
    private val context: Context get() = application.applicationContext

}