# Signing Windows builds

CI signs `frog.exe` with [Azure Artifact Signing](https://learn.microsoft.com/azure/artifact-signing/) on pushes to `main`. Pull requests and other branches are built unsigned. The certificate is issued to an individual developer, so the signature shows that person's legal name.

GitHub signs in to Azure over OpenID Connect (OIDC). No Azure secret is stored in the repository.

## One-time setup in Azure

Individual validation is available in the US and Canada only.

1. **Subscription.** Use an Azure subscription whose billing account is of type **Individual**, with your legal name and address as they appear on your government ID. Artifact Signing takes the certificate's details from it.
2. **Resource provider.** In the subscription, register the `Microsoft.CodeSigning` resource provider.
3. **Signing account.** Create an Artifact Signing account. Note its region and its **Account URI**, for example `https://eus.codesigning.azure.net/`. The URI must match the account's region.
4. **Identity validation.** In the account, create an identity validation request of type **Public**, as an **Individual**. Enter your name exactly as it is on your ID. Complete the Microsoft Entra Verified ID check in the Microsoft Authenticator app (a scan of your ID and a selfie). Validation can take from minutes to days.
5. **Certificate profile.** After validation passes, create a **Public Trust** certificate profile linked to it. Use the subject preview to check the name.
6. **App registration for GitHub.** In Microsoft Entra ID, register an app, for example `frog-github-signing`. Under **Certificates & secrets → Federated credentials**, add a GitHub Actions credential:
   - Organization: `hitoma-apps`
   - Repository: `frog`
   - Entity type: **Branch**, `main`

   This lets only workflows on `main` sign in as this app.
7. **Role.** On the certificate profile (or the signing account), assign the app the **Artifact Signing Certificate Profile Signer** role. It needs no other access.

## Repository variables

In GitHub, set these under **Settings → Secrets and variables → Actions → Variables**. None of them is secret: they name the resources, and the federated credential controls who can use them.

| Variable | Value |
|---|---|
| `AZURE_CLIENT_ID` | The app registration's application (client) ID |
| `AZURE_TENANT_ID` | The Entra tenant (directory) ID |
| `AZURE_SUBSCRIPTION_ID` | The subscription ID |
| `ARTIFACT_SIGNING_ENDPOINT` | The signing account's URI, for example `https://eus.codesigning.azure.net/` |
| `ARTIFACT_SIGNING_ACCOUNT` | The signing account's name |
| `ARTIFACT_SIGNING_PROFILE` | The certificate profile's name |

Signing stays off until `AZURE_CLIENT_ID` is set. Once it is, the next push to `main` signs the build, and CI fails if the signature isn't valid.

## Renewal

Identity validation expires. Renew it before the expiry date, or signing stops.
