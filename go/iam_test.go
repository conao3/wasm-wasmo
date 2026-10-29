package wasmo_test

import (
	"context"
	"encoding/json"
	"errors"
	"net/url"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/iam"
	"github.com/aws/aws-sdk-go-v2/service/iam/types"
)

const trustEC2 = `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}`

func newIAM(t *testing.T) *iam.Client {
	t.Helper()
	return iam.NewFromConfig(newMock(t).Config())
}

func managedPolicy(t *testing.T, client *iam.Client, name, doc string) string {
	t.Helper()
	out, err := client.CreatePolicy(context.Background(), &iam.CreatePolicyInput{PolicyName: aws.String(name), PolicyDocument: aws.String(doc)})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.Policy.Arn)
}

func decision(t *testing.T, out *iam.SimulatePrincipalPolicyOutput, action, resource string) types.PolicyEvaluationDecisionType {
	t.Helper()
	for _, r := range out.EvaluationResults {
		if aws.ToString(r.EvalActionName) == action && aws.ToString(r.EvalResourceName) == resource {
			return r.EvalDecision
		}
	}
	t.Fatalf("no result for %s on %s", action, resource)
	return ""
}

func TestIAMRolesAndTrustPolicies(t *testing.T) {
	ctx := context.Background()
	client := newIAM(t)
	created, err := client.CreateRole(ctx, &iam.CreateRoleInput{
		RoleName:                 aws.String("app"),
		Path:                     aws.String("/service/"),
		AssumeRolePolicyDocument: aws.String(trustEC2),
		Description:              aws.String("application role"),
		Tags:                     []types.Tag{{Key: aws.String("team"), Value: aws.String("core")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	role := created.Role
	if aws.ToString(role.Arn) != "arn:aws:iam::000000000000:role/service/app" || !strings.HasPrefix(aws.ToString(role.RoleId), "AROA") || aws.ToInt32(role.MaxSessionDuration) != 3600 {
		t.Fatalf("role = %+v", role)
	}
	got, err := client.GetRole(ctx, &iam.GetRoleInput{RoleName: aws.String("app")})
	if err != nil {
		t.Fatal(err)
	}
	doc, err := url.QueryUnescape(aws.ToString(got.Role.AssumeRolePolicyDocument))
	if err != nil || doc != trustEC2 {
		t.Fatalf("trust policy = %q (%v)", aws.ToString(got.Role.AssumeRolePolicyDocument), err)
	}
	if len(got.Role.Tags) != 1 || aws.ToString(got.Role.Description) != "application role" {
		t.Fatalf("role = %+v", got.Role)
	}

	newTrust := strings.Replace(trustEC2, "ec2", "lambda", 1)
	if _, err := client.UpdateAssumeRolePolicy(ctx, &iam.UpdateAssumeRolePolicyInput{RoleName: aws.String("app"), PolicyDocument: aws.String(newTrust)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UpdateRole(ctx, &iam.UpdateRoleInput{RoleName: aws.String("app"), MaxSessionDuration: aws.Int32(7200)}); err != nil {
		t.Fatal(err)
	}
	got, _ = client.GetRole(ctx, &iam.GetRoleInput{RoleName: aws.String("app")})
	if doc, _ := url.QueryUnescape(aws.ToString(got.Role.AssumeRolePolicyDocument)); doc != newTrust || aws.ToInt32(got.Role.MaxSessionDuration) != 7200 {
		t.Fatalf("updated role = %+v", got.Role)
	}

	var exists *types.EntityAlreadyExistsException
	if _, err := client.CreateRole(ctx, &iam.CreateRoleInput{RoleName: aws.String("app"), AssumeRolePolicyDocument: aws.String(trustEC2)}); !errors.As(err, &exists) {
		t.Fatalf("duplicate role: %v", err)
	}
	var malformed *types.MalformedPolicyDocumentException
	if _, err := client.CreateRole(ctx, &iam.CreateRoleInput{RoleName: aws.String("bad"), AssumeRolePolicyDocument: aws.String("{")}); !errors.As(err, &malformed) {
		t.Fatalf("malformed trust: %v", err)
	}

	profile, err := client.CreateInstanceProfile(ctx, &iam.CreateInstanceProfileInput{InstanceProfileName: aws.String("app-profile")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.AddRoleToInstanceProfile(ctx, &iam.AddRoleToInstanceProfileInput{InstanceProfileName: aws.String("app-profile"), RoleName: aws.String("app")}); err != nil {
		t.Fatal(err)
	}
	profiles, err := client.ListInstanceProfilesForRole(ctx, &iam.ListInstanceProfilesForRoleInput{RoleName: aws.String("app")})
	if err != nil {
		t.Fatal(err)
	}
	if len(profiles.InstanceProfiles) != 1 || aws.ToString(profiles.InstanceProfiles[0].InstanceProfileId) != aws.ToString(profile.InstanceProfile.InstanceProfileId) || len(profiles.InstanceProfiles[0].Roles) != 1 {
		t.Fatalf("profiles = %+v", profiles.InstanceProfiles)
	}
	var conflict *types.DeleteConflictException
	if _, err := client.DeleteRole(ctx, &iam.DeleteRoleInput{RoleName: aws.String("app")}); !errors.As(err, &conflict) {
		t.Fatalf("delete role in profile: %v", err)
	}
	if _, err := client.RemoveRoleFromInstanceProfile(ctx, &iam.RemoveRoleFromInstanceProfileInput{InstanceProfileName: aws.String("app-profile"), RoleName: aws.String("app")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DeleteRole(ctx, &iam.DeleteRoleInput{RoleName: aws.String("app")}); err != nil {
		t.Fatal(err)
	}
	var nf *types.NoSuchEntityException
	if _, err := client.GetRole(ctx, &iam.GetRoleInput{RoleName: aws.String("app")}); !errors.As(err, &nf) || httpStatus(err) != 404 {
		t.Fatalf("deleted role: %v", err)
	}

	linked, err := client.CreateServiceLinkedRole(ctx, &iam.CreateServiceLinkedRoleInput{AWSServiceName: aws.String("elasticbeanstalk.amazonaws.com")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(linked.Role.Path) != "/aws-service-role/elasticbeanstalk.amazonaws.com/" || !strings.HasPrefix(aws.ToString(linked.Role.RoleName), "AWSServiceRoleFor") {
		t.Fatalf("service-linked role = %+v", linked.Role)
	}
}

func TestIAMManagedPoliciesAndVersions(t *testing.T) {
	ctx := context.Background()
	client := newIAM(t)
	v1 := `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:GetObject","Resource":"*"}]}`
	v2 := `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","s3:PutObject"],"Resource":"*"}]}`
	arn := managedPolicy(t, client, "s3-access", v1)
	if arn != "arn:aws:iam::000000000000:policy/s3-access" {
		t.Fatalf("arn = %s", arn)
	}
	if _, err := client.CreatePolicyVersion(ctx, &iam.CreatePolicyVersionInput{PolicyArn: aws.String(arn), PolicyDocument: aws.String(v2), SetAsDefault: true}); err != nil {
		t.Fatal(err)
	}
	policy, err := client.GetPolicy(ctx, &iam.GetPolicyInput{PolicyArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(policy.Policy.DefaultVersionId) != "v2" {
		t.Fatalf("default = %s", aws.ToString(policy.Policy.DefaultVersionId))
	}
	version, err := client.GetPolicyVersion(ctx, &iam.GetPolicyVersionInput{PolicyArn: aws.String(arn), VersionId: aws.String("v2")})
	if err != nil {
		t.Fatal(err)
	}
	if doc, _ := url.QueryUnescape(aws.ToString(version.PolicyVersion.Document)); doc != v2 || !version.PolicyVersion.IsDefaultVersion {
		t.Fatalf("version = %+v", version.PolicyVersion)
	}
	var conflict *types.DeleteConflictException
	if _, err := client.DeletePolicyVersion(ctx, &iam.DeletePolicyVersionInput{PolicyArn: aws.String(arn), VersionId: aws.String("v2")}); !errors.As(err, &conflict) {
		t.Fatalf("delete default version: %v", err)
	}
	if _, err := client.SetDefaultPolicyVersion(ctx, &iam.SetDefaultPolicyVersionInput{PolicyArn: aws.String(arn), VersionId: aws.String("v1")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DeletePolicyVersion(ctx, &iam.DeletePolicyVersionInput{PolicyArn: aws.String(arn), VersionId: aws.String("v2")}); err != nil {
		t.Fatal(err)
	}
	versions, err := client.ListPolicyVersions(ctx, &iam.ListPolicyVersionsInput{PolicyArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if len(versions.Versions) != 1 {
		t.Fatalf("versions = %+v", versions.Versions)
	}

	if _, err := client.CreateRole(ctx, &iam.CreateRoleInput{RoleName: aws.String("reader"), AssumeRolePolicyDocument: aws.String(trustEC2)}); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{arn, "arn:aws:iam::aws:policy/ReadOnlyAccess"} {
		if _, err := client.AttachRolePolicy(ctx, &iam.AttachRolePolicyInput{RoleName: aws.String("reader"), PolicyArn: aws.String(p)}); err != nil {
			t.Fatal(err)
		}
	}
	var nf *types.NoSuchEntityException
	if _, err := client.AttachRolePolicy(ctx, &iam.AttachRolePolicyInput{RoleName: aws.String("reader"), PolicyArn: aws.String("arn:aws:iam::aws:policy/NoSuchThing")}); !errors.As(err, &nf) {
		t.Fatalf("attach unknown policy: %v", err)
	}
	attached, err := client.ListAttachedRolePolicies(ctx, &iam.ListAttachedRolePoliciesInput{RoleName: aws.String("reader")})
	if err != nil {
		t.Fatal(err)
	}
	if len(attached.AttachedPolicies) != 2 {
		t.Fatalf("attached = %+v", attached.AttachedPolicies)
	}
	policy, _ = client.GetPolicy(ctx, &iam.GetPolicyInput{PolicyArn: aws.String(arn)})
	if aws.ToInt32(policy.Policy.AttachmentCount) != 1 {
		t.Fatalf("attachment count = %d", aws.ToInt32(policy.Policy.AttachmentCount))
	}
	entities, err := client.ListEntitiesForPolicy(ctx, &iam.ListEntitiesForPolicyInput{PolicyArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if len(entities.PolicyRoles) != 1 || aws.ToString(entities.PolicyRoles[0].RoleName) != "reader" {
		t.Fatalf("entities = %+v", entities)
	}
	if _, err := client.DeletePolicy(ctx, &iam.DeletePolicyInput{PolicyArn: aws.String(arn)}); !errors.As(err, &conflict) {
		t.Fatalf("delete attached policy: %v", err)
	}
	local, err := client.ListPolicies(ctx, &iam.ListPoliciesInput{Scope: types.PolicyScopeTypeLocal})
	if err != nil {
		t.Fatal(err)
	}
	if len(local.Policies) != 1 {
		t.Fatalf("local policies = %+v", local.Policies)
	}

	if _, err := client.PutRolePolicy(ctx, &iam.PutRolePolicyInput{RoleName: aws.String("reader"), PolicyName: aws.String("inline"), PolicyDocument: aws.String(v1)}); err != nil {
		t.Fatal(err)
	}
	inline, err := client.GetRolePolicy(ctx, &iam.GetRolePolicyInput{RoleName: aws.String("reader"), PolicyName: aws.String("inline")})
	if err != nil {
		t.Fatal(err)
	}
	if doc, _ := url.QueryUnescape(aws.ToString(inline.PolicyDocument)); doc != v1 {
		t.Fatalf("inline = %s", doc)
	}
	names, err := client.ListRolePolicies(ctx, &iam.ListRolePoliciesInput{RoleName: aws.String("reader")})
	if err != nil {
		t.Fatal(err)
	}
	if len(names.PolicyNames) != 1 || names.PolicyNames[0] != "inline" {
		t.Fatalf("inline names = %v", names.PolicyNames)
	}
}

func TestIAMUsersGroupsAndCredentials(t *testing.T) {
	ctx := context.Background()
	client := newIAM(t)
	if _, err := client.CreateUser(ctx, &iam.CreateUserInput{UserName: aws.String("alice"), Tags: []types.Tag{{Key: aws.String("dept"), Value: aws.String("eng")}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.CreateGroup(ctx, &iam.CreateGroupInput{GroupName: aws.String("devs")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.AddUserToGroup(ctx, &iam.AddUserToGroupInput{GroupName: aws.String("devs"), UserName: aws.String("alice")}); err != nil {
		t.Fatal(err)
	}
	group, err := client.GetGroup(ctx, &iam.GetGroupInput{GroupName: aws.String("devs")})
	if err != nil {
		t.Fatal(err)
	}
	if len(group.Users) != 1 || aws.ToString(group.Users[0].UserName) != "alice" {
		t.Fatalf("group = %+v", group)
	}
	groups, err := client.ListGroupsForUser(ctx, &iam.ListGroupsForUserInput{UserName: aws.String("alice")})
	if err != nil {
		t.Fatal(err)
	}
	if len(groups.Groups) != 1 {
		t.Fatalf("groups = %+v", groups.Groups)
	}

	var keys []string
	for range 2 {
		k, err := client.CreateAccessKey(ctx, &iam.CreateAccessKeyInput{UserName: aws.String("alice")})
		if err != nil {
			t.Fatal(err)
		}
		if !strings.HasPrefix(aws.ToString(k.AccessKey.AccessKeyId), "AKIA") || len(aws.ToString(k.AccessKey.SecretAccessKey)) != 40 {
			t.Fatalf("key = %+v", k.AccessKey)
		}
		keys = append(keys, aws.ToString(k.AccessKey.AccessKeyId))
	}
	var limit *types.LimitExceededException
	if _, err := client.CreateAccessKey(ctx, &iam.CreateAccessKeyInput{UserName: aws.String("alice")}); !errors.As(err, &limit) {
		t.Fatalf("third key: %v", err)
	}
	if _, err := client.UpdateAccessKey(ctx, &iam.UpdateAccessKeyInput{UserName: aws.String("alice"), AccessKeyId: aws.String(keys[0]), Status: types.StatusTypeInactive}); err != nil {
		t.Fatal(err)
	}
	listed, err := client.ListAccessKeys(ctx, &iam.ListAccessKeysInput{UserName: aws.String("alice")})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.AccessKeyMetadata) != 2 || listed.AccessKeyMetadata[0].Status != types.StatusTypeInactive {
		t.Fatalf("keys = %+v", listed.AccessKeyMetadata)
	}

	if _, err := client.UpdateAccountPasswordPolicy(ctx, &iam.UpdateAccountPasswordPolicyInput{MinimumPasswordLength: aws.Int32(12), RequireNumbers: true}); err != nil {
		t.Fatal(err)
	}
	var weak *types.PasswordPolicyViolationException
	if _, err := client.CreateLoginProfile(ctx, &iam.CreateLoginProfileInput{UserName: aws.String("alice"), Password: aws.String("short")}); !errors.As(err, &weak) {
		t.Fatalf("weak password: %v", err)
	}
	if _, err := client.CreateLoginProfile(ctx, &iam.CreateLoginProfileInput{UserName: aws.String("alice"), Password: aws.String("correct-horse-42")}); err != nil {
		t.Fatal(err)
	}

	var conflict *types.DeleteConflictException
	if _, err := client.DeleteUser(ctx, &iam.DeleteUserInput{UserName: aws.String("alice")}); !errors.As(err, &conflict) {
		t.Fatalf("delete user with credentials: %v", err)
	}

	if _, err := client.GenerateCredentialReport(ctx, &iam.GenerateCredentialReportInput{}); err != nil {
		t.Fatal(err)
	}
	report, err := client.GetCredentialReport(ctx, &iam.GetCredentialReportInput{})
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(report.Content)), "\n")
	if len(lines) != 3 || !strings.HasPrefix(lines[2], "alice,arn:aws:iam::000000000000:user/alice,") || !strings.Contains(lines[2], ",true,") {
		t.Fatalf("report = %q", report.Content)
	}

	if _, err := client.CreateAccountAlias(ctx, &iam.CreateAccountAliasInput{AccountAlias: aws.String("wasmo-test")}); err != nil {
		t.Fatal(err)
	}
	aliases, err := client.ListAccountAliases(ctx, &iam.ListAccountAliasesInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(aliases.AccountAliases) != 1 || aliases.AccountAliases[0] != "wasmo-test" {
		t.Fatalf("aliases = %v", aliases.AccountAliases)
	}
	summary, err := client.GetAccountSummary(ctx, &iam.GetAccountSummaryInput{})
	if err != nil {
		t.Fatal(err)
	}
	if summary.SummaryMap["Users"] != 1 || summary.SummaryMap["Groups"] != 1 {
		t.Fatalf("summary = %v", summary.SummaryMap)
	}
}

func TestIAMIdentityProviders(t *testing.T) {
	ctx := context.Background()
	client := newIAM(t)
	oidc, err := client.CreateOpenIDConnectProvider(ctx, &iam.CreateOpenIDConnectProviderInput{
		Url:            aws.String("https://token.actions.githubusercontent.com"),
		ClientIDList:   []string{"sts.amazonaws.com"},
		ThumbprintList: []string{strings.Repeat("a", 40)},
	})
	if err != nil {
		t.Fatal(err)
	}
	arn := aws.ToString(oidc.OpenIDConnectProviderArn)
	if arn != "arn:aws:iam::000000000000:oidc-provider/token.actions.githubusercontent.com" {
		t.Fatalf("arn = %s", arn)
	}
	if _, err := client.AddClientIDToOpenIDConnectProvider(ctx, &iam.AddClientIDToOpenIDConnectProviderInput{OpenIDConnectProviderArn: aws.String(arn), ClientID: aws.String("other")}); err != nil {
		t.Fatal(err)
	}
	got, err := client.GetOpenIDConnectProvider(ctx, &iam.GetOpenIDConnectProviderInput{OpenIDConnectProviderArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.ClientIDList) != 2 || aws.ToString(got.Url) != "token.actions.githubusercontent.com" {
		t.Fatalf("provider = %+v", got)
	}
	metadata := `<?xml version="1.0"?><md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="https://idp.example.com"></md:EntityDescriptor>`
	saml, err := client.CreateSAMLProvider(ctx, &iam.CreateSAMLProviderInput{Name: aws.String("corp"), SAMLMetadataDocument: aws.String(metadata)})
	if err != nil {
		t.Fatal(err)
	}
	providers, err := client.ListSAMLProviders(ctx, &iam.ListSAMLProvidersInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(providers.SAMLProviderList) != 1 || aws.ToString(providers.SAMLProviderList[0].Arn) != aws.ToString(saml.SAMLProviderArn) {
		t.Fatalf("saml = %+v", providers.SAMLProviderList)
	}
	if _, err := client.DeleteSAMLProvider(ctx, &iam.DeleteSAMLProviderInput{SAMLProviderArn: saml.SAMLProviderArn}); err != nil {
		t.Fatal(err)
	}
}

func TestIAMSimulatePrincipalPolicy(t *testing.T) {
	ctx := context.Background()
	client := newIAM(t)
	if _, err := client.CreateUser(ctx, &iam.CreateUserInput{UserName: aws.String("bob")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.CreateGroup(ctx, &iam.CreateGroupInput{GroupName: aws.String("readers")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.AddUserToGroup(ctx, &iam.AddUserToGroupInput{GroupName: aws.String("readers"), UserName: aws.String("bob")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.AttachGroupPolicy(ctx, &iam.AttachGroupPolicyInput{GroupName: aws.String("readers"), PolicyArn: aws.String("arn:aws:iam::aws:policy/AmazonS3FullAccess")}); err != nil {
		t.Fatal(err)
	}
	deny := `{"Version":"2012-10-17","Statement":[{"Effect":"Deny","Action":"s3:DeleteObject","Resource":"arn:aws:s3:::prod/*"}]}`
	if _, err := client.PutUserPolicy(ctx, &iam.PutUserPolicyInput{UserName: aws.String("bob"), PolicyName: aws.String("no-prod-delete"), PolicyDocument: aws.String(deny)}); err != nil {
		t.Fatal(err)
	}
	user, err := client.GetUser(ctx, &iam.GetUserInput{UserName: aws.String("bob")})
	if err != nil {
		t.Fatal(err)
	}
	simulate := func() *iam.SimulatePrincipalPolicyOutput {
		t.Helper()
		out, err := client.SimulatePrincipalPolicy(ctx, &iam.SimulatePrincipalPolicyInput{
			PolicySourceArn: user.User.Arn,
			ActionNames:     []string{"s3:GetObject", "s3:DeleteObject", "dynamodb:GetItem"},
			ResourceArns:    []string{"arn:aws:s3:::prod/a", "arn:aws:s3:::dev/a"},
		})
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	out := simulate()
	if len(out.EvaluationResults) != 6 {
		t.Fatalf("results = %d", len(out.EvaluationResults))
	}
	for _, c := range []struct {
		action, resource string
		want             types.PolicyEvaluationDecisionType
	}{
		{"s3:GetObject", "arn:aws:s3:::prod/a", types.PolicyEvaluationDecisionTypeAllowed},
		{"s3:DeleteObject", "arn:aws:s3:::prod/a", types.PolicyEvaluationDecisionTypeExplicitDeny},
		{"s3:DeleteObject", "arn:aws:s3:::dev/a", types.PolicyEvaluationDecisionTypeAllowed},
		{"dynamodb:GetItem", "arn:aws:s3:::dev/a", types.PolicyEvaluationDecisionTypeImplicitDeny},
	} {
		if got := decision(t, out, c.action, c.resource); got != c.want {
			t.Fatalf("%s on %s = %s, want %s", c.action, c.resource, got, c.want)
		}
	}

	boundary := managedPolicy(t, client, "read-only-boundary", `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:Get*","Resource":"*"}]}`)
	if _, err := client.PutUserPermissionsBoundary(ctx, &iam.PutUserPermissionsBoundaryInput{UserName: aws.String("bob"), PermissionsBoundary: aws.String(boundary)}); err != nil {
		t.Fatal(err)
	}
	out = simulate()
	if got := decision(t, out, "s3:DeleteObject", "arn:aws:s3:::dev/a"); got != types.PolicyEvaluationDecisionTypeImplicitDeny {
		t.Fatalf("boundary should cap DeleteObject, got %s", got)
	}
	if got := decision(t, out, "s3:GetObject", "arn:aws:s3:::dev/a"); got != types.PolicyEvaluationDecisionTypeAllowed {
		t.Fatalf("GetObject inside boundary = %s", got)
	}

	custom, err := client.SimulateCustomPolicy(ctx, &iam.SimulateCustomPolicyInput{
		PolicyInputList: []string{`{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"sqs:SendMessage","Resource":"*","Condition":{"StringEquals":{"aws:RequestedRegion":"us-east-1"}}}]}`},
		ActionNames:     []string{"sqs:SendMessage"},
		ContextEntries:  []types.ContextEntry{{ContextKeyName: aws.String("aws:RequestedRegion"), ContextKeyType: types.ContextKeyTypeEnumString, ContextKeyValues: []string{"eu-west-1"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if custom.EvaluationResults[0].EvalDecision != types.PolicyEvaluationDecisionTypeImplicitDeny {
		t.Fatalf("condition mismatch = %s", custom.EvaluationResults[0].EvalDecision)
	}
	keys, err := client.GetContextKeysForPrincipalPolicy(ctx, &iam.GetContextKeysForPrincipalPolicyInput{
		PolicySourceArn: user.User.Arn,
		PolicyInputList: []string{`{"Statement":[{"Effect":"Allow","Action":"*","Resource":"*","Condition":{"Bool":{"aws:SecureTransport":"true"}}}]}`},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(keys.ContextKeyNames) != 1 || keys.ContextKeyNames[0] != "aws:SecureTransport" {
		t.Fatalf("context keys = %v", keys.ContextKeyNames)
	}

	details, err := client.GetAccountAuthorizationDetails(ctx, &iam.GetAccountAuthorizationDetailsInput{Filter: []types.EntityType{types.EntityTypeUser}})
	if err != nil {
		t.Fatal(err)
	}
	if len(details.UserDetailList) != 1 || len(details.UserDetailList[0].UserPolicyList) != 1 || details.UserDetailList[0].GroupList[0] != "readers" {
		raw, _ := json.Marshal(details.UserDetailList)
		t.Fatalf("details = %s", raw)
	}
}
