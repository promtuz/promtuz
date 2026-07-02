package com.promtuz.chat.ui.components

import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R

@Composable
fun GoBackButton(modifier: Modifier = Modifier) {
    val backHandler = LocalOnBackPressedDispatcherOwner.current
    IconButton({
        backHandler?.onBackPressedDispatcher?.onBackPressed()
    }, modifier) {
        Icon(
            painter = painterResource(R.drawable.i_back),
            "Go Back",
            Modifier.size(28.dp),
            MaterialTheme.colorScheme.onSurface
        )
    }
}