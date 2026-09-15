package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.FlexibleScreen
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen

@Composable
fun IdentityKeysScreen(onShareIdentity: () -> Unit, onRecoveryPhrase: () -> Unit) {
    val direction = LocalLayoutDirection.current
    SimpleScreen({ Text(stringResource(R.string.identity_keys_title)) }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize()
                .padding(start = padding.calculateLeftPadding(direction), end = padding.calculateRightPadding(direction)),
            contentPadding = PaddingValues(18.dp, padding.calculateTopPadding() + 12.dp,
                18.dp, padding.calculateBottomPadding() + 32.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            item {
                GroupedActionRow(stringResource(R.string.identity_qr_title), 0, 2, onShareIdentity,
                    supportingText = stringResource(R.string.identity_qr_body)) {
                    DrawableIcon(R.drawable.i_qr_code, size = 26.dp)
                }
            }
            item {
                GroupedActionRow(stringResource(R.string.recovery_title), 1, 2, onRecoveryPhrase,
                    supportingText = stringResource(R.string.identity_recovery_body)) {
                    DrawableIcon(R.drawable.i_key, size = 26.dp)
                }
            }
            item {
                Text(stringResource(R.string.identity_history_note), Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                    style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}
