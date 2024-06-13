package net.mullvad.mullvadvpn.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import net.mullvad.mullvadvpn.compose.state.WelcomeUiState
import net.mullvad.mullvadvpn.constant.ACCOUNT_EXPIRY_POLL_INTERVAL
import net.mullvad.mullvadvpn.lib.model.WebsiteAuthToken
import net.mullvad.mullvadvpn.lib.shared.AccountRepository
import net.mullvad.mullvadvpn.lib.shared.ConnectionProxy
import net.mullvad.mullvadvpn.lib.shared.DeviceRepository
import net.mullvad.mullvadvpn.usecase.PaymentUseCase
import net.mullvad.mullvadvpn.util.toPaymentState

class WelcomeViewModel(
    private val accountRepository: AccountRepository,
    deviceRepository: DeviceRepository,
    private val paymentUseCase: PaymentUseCase,
    connectionProxy: ConnectionProxy,
    private val pollAccountExpiry: Boolean = true,
    private val isPlayBuild: Boolean
) : ViewModel() {
    private val _uiSideEffect = Channel<UiSideEffect>()
    val uiSideEffect = merge(_uiSideEffect.receiveAsFlow(), hasAddedTimeEffect())

    val uiState =
        combine(
                connectionProxy.tunnelState,
                deviceRepository.deviceState.filterNotNull(),
                paymentUseCase.paymentAvailability,
            ) { tunnelState, accountState, paymentAvailability ->
                WelcomeUiState(
                    tunnelState = tunnelState,
                    accountNumber = accountState.token(),
                    deviceName = accountState.displayName(),
                    showSitePayment = !isPlayBuild,
                    billingPaymentState = paymentAvailability?.toPaymentState(),
                )
            }
            .stateIn(viewModelScope, SharingStarted.WhileSubscribed(), WelcomeUiState())

    init {
        viewModelScope.launch {
            while (pollAccountExpiry) {
                updateAccountExpiry()
                delay(ACCOUNT_EXPIRY_POLL_INTERVAL)
            }
        }
        verifyPurchases()
        fetchPaymentAvailability()
        viewModelScope.launch { deviceRepository.updateDevice() }
    }

    private fun hasAddedTimeEffect() =
        accountRepository.accountData
            .filterNotNull()
            .filter { it.expiryDate.minusHours(MIN_HOURS_PAST_ACCOUNT_EXPIRY).isAfterNow }
            .onEach { paymentUseCase.resetPurchaseResult() }
            .map { UiSideEffect.OpenConnectScreen }

    fun onSitePaymentClick() {
        viewModelScope.launch {
            accountRepository.getWebsiteAuthToken()?.let { token ->
                _uiSideEffect.send(UiSideEffect.OpenAccountView(token))
            }
        }
    }

    private fun verifyPurchases() {
        viewModelScope.launch {
            paymentUseCase.verifyPurchases()
            updateAccountExpiry()
        }
    }

    private fun fetchPaymentAvailability() {
        viewModelScope.launch { paymentUseCase.queryPaymentAvailability() }
    }

    fun onClosePurchaseResultDialog(success: Boolean) {
        // We are closing the dialog without any action, this can happen either if an error occurred
        // during the purchase or the purchase ended successfully.
        // If the payment was successful we want to update the account expiry. If not successful we
        // should check payment availability and verify any purchases to handle potential errors.
        if (success) {
            viewModelScope.launch { updateAccountExpiry() }
            // Emission of out of time navigation is handled by launch in onStart
        } else {
            fetchPaymentAvailability()
            verifyPurchases() // Attempt to verify again
        }
        viewModelScope.launch {
            paymentUseCase.resetPurchaseResult() // So that we do not show the dialog again.
        }
    }

    private suspend fun updateAccountExpiry() {
        accountRepository.getAccountData()
    }

    sealed interface UiSideEffect {
        data class OpenAccountView(val token: WebsiteAuthToken) : UiSideEffect

        data object OpenConnectScreen : UiSideEffect
    }

    companion object {
        private const val MIN_HOURS_PAST_ACCOUNT_EXPIRY = 20
    }
}
