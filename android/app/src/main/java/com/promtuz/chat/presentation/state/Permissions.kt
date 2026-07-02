package com.promtuz.chat.presentation.state

import android.content.pm.PackageManager

enum class PermissionState {
    Granted,
    Denied,
    NotRequested;

    companion object {
        fun from(requestResult: Int): PermissionState {
            return when (requestResult) {
                PackageManager.PERMISSION_DENIED -> Denied
                PackageManager.PERMISSION_GRANTED -> Granted
                else -> NotRequested
            }
        }
    }
}

