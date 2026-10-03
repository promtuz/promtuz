package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.components.AppAlertDialog
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SettingsSection
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.launch
import uniffi.core.BlockedPerson

@Composable
fun PrivacySettingsScreen() {
    val scope = rememberCoroutineScope()
    var everyone by remember { mutableStateOf<Boolean?>(null) }
    LaunchedEffect(Unit) { everyone = runCatching { CoreBridge.messageRequestsEnabled() }.getOrDefault(true) }
    val blocked by remember { observeQuery(setOf("prefs")) { CoreBridge.blockedPeople() } }.collectAsState(emptyList())
    var unblocking by remember { mutableStateOf<BlockedPerson?>(null) }

    SimpleScreen({ Text("Privacy") }) { padding ->
        Column(
            Modifier.fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(padding.listPadding()),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            SettingsSection("Who can message me")
            Column(Modifier.selectableGroup(), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                listOf(true, false).forEachIndexed { index, option ->
                    GroupedActionRow(
                        title = if (option) "Everyone" else "My contacts",
                        supportingText = if (option) "People you haven’t added go to Message requests"
                                 else "Only people you’ve added can message you",
                        index = index,
                        groupSize = 2,
                        selected = everyone == option,
                        onClick = {
                            everyone = option
                            scope.launch { runCatching { CoreBridge.setMessageRequestsEnabled(option) } }
                        },
                        control = { RadioButton(selected = everyone == option, onClick = null) },
                    )
                }
            }
            if (blocked.isNotEmpty()) {
                SettingsSection("Blocked")
                blocked.forEachIndexed { index, person ->
                    GroupedActionRow(
                        title = person.name,
                        index = index,
                        groupSize = blocked.size,
                        onClick = { unblocking = person },
                        control = { Text("Unblock", color = MaterialTheme.colorScheme.primary, style = MaterialTheme.typography.labelLarge) },
                    )
                }
            }
        }
    }

    unblocking?.let { person ->
        AppAlertDialog(
            onDismissRequest = { unblocking = null },
            title = { Text("Unblock ${person.name}?") },
            text = { Text("They’ll be able to send you message requests again.") },
            confirmButton = {
                TextButton(onClick = {
                    unblocking = null
                    scope.launch { runCatching { CoreBridge.unblock(person.ipk) } }
                }) { Text("Unblock") }
            },
            dismissButton = { TextButton(onClick = { unblocking = null }) { Text("Cancel") } },
        )
    }
}
