package wasmo_test

import (
	"context"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/secretsmanager"
	"github.com/aws/aws-sdk-go-v2/service/secretsmanager/types"
)

func smClient(t *testing.T) *secretsmanager.Client {
	t.Helper()
	return secretsmanager.NewFromConfig(newMock(t).Config())
}

func mustSecret(t *testing.T, client *secretsmanager.Client, name, value string) *secretsmanager.CreateSecretOutput {
	t.Helper()
	out, err := client.CreateSecret(context.Background(), &secretsmanager.CreateSecretInput{
		Name:         aws.String(name),
		SecretString: aws.String(value),
	})
	if err != nil {
		t.Fatal(err)
	}
	return out
}

func TestSecretsManagerLifecycle(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)

	created, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{
		Name:         aws.String("app/db"),
		Description:  aws.String("database"),
		SecretString: aws.String(`{"user":"admin"}`),
		Tags:         []types.Tag{{Key: aws.String("env"), Value: aws.String("dev")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(aws.ToString(created.ARN), "arn:aws:secretsmanager:us-east-1:000000000000:secret:app/db-") {
		t.Fatalf("arn = %q", aws.ToString(created.ARN))
	}

	got, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: created.ARN})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.SecretString) != `{"user":"admin"}` || aws.ToString(got.VersionId) != aws.ToString(created.VersionId) {
		t.Fatalf("value = %+v", got)
	}
	if !slices.Equal(got.VersionStages, []string{"AWSCURRENT"}) {
		t.Fatalf("stages = %v", got.VersionStages)
	}

	updated, err := client.UpdateSecret(ctx, &secretsmanager.UpdateSecretInput{
		SecretId:     aws.String("app/db"),
		Description:  aws.String("primary database"),
		SecretString: aws.String("v2"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if updated.VersionId == nil || aws.ToString(updated.VersionId) == aws.ToString(created.VersionId) {
		t.Fatalf("update version = %v", aws.ToString(updated.VersionId))
	}

	desc, err := client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("app/db")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(desc.Description) != "primary database" || len(desc.VersionIdsToStages) != 2 || desc.CreatedDate == nil {
		t.Fatalf("describe = %+v", desc)
	}
	if time.Since(*desc.CreatedDate) > time.Minute {
		t.Fatalf("created = %v", desc.CreatedDate)
	}

	bin, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{
		Name:         aws.String("app/key"),
		SecretBinary: []byte{0, 1, 2, 0xff},
	})
	if err != nil {
		t.Fatal(err)
	}
	gotBin, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: bin.Name})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(gotBin.SecretBinary, []byte{0, 1, 2, 0xff}) || gotBin.SecretString != nil {
		t.Fatalf("binary = %v", gotBin.SecretBinary)
	}

	for i := range 3 {
		mustSecret(t, client, "page/"+string(rune('a'+i)), "x")
	}
	var names []string
	pager := secretsmanager.NewListSecretsPaginator(client, &secretsmanager.ListSecretsInput{
		MaxResults: aws.Int32(2),
		SortBy:     types.SortByTypeName,
		SortOrder:  types.SortOrderTypeDesc,
	})
	for pager.HasMorePages() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if len(page.SecretList) > 2 {
			t.Fatalf("page size = %d", len(page.SecretList))
		}
		for _, s := range page.SecretList {
			names = append(names, aws.ToString(s.Name))
		}
	}
	if !slices.Equal(names, []string{"page/c", "page/b", "page/a", "app/key", "app/db"}) {
		t.Fatalf("names = %v", names)
	}

	filtered, err := client.ListSecrets(ctx, &secretsmanager.ListSecretsInput{
		Filters: []types.Filter{{Key: types.FilterNameStringTypeTagKey, Values: []string{"env"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(filtered.SecretList) != 1 || aws.ToString(filtered.SecretList[0].Name) != "app/db" {
		t.Fatalf("filtered = %+v", filtered.SecretList)
	}
	if len(filtered.SecretList[0].SecretVersionsToStages) != 2 {
		t.Fatalf("list stages = %v", filtered.SecretList[0].SecretVersionsToStages)
	}
}

func TestSecretsManagerVersionsAndStages(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	created := mustSecret(t, client, "rotating", "v1")

	put, err := client.PutSecretValue(ctx, &secretsmanager.PutSecretValueInput{
		SecretId:           aws.String("rotating"),
		SecretString:       aws.String("v2"),
		ClientRequestToken: aws.String("token-v2-0000000000000000000000000"),
	})
	if err != nil {
		t.Fatal(err)
	}
	again, err := client.PutSecretValue(ctx, &secretsmanager.PutSecretValueInput{
		SecretId:           aws.String("rotating"),
		SecretString:       aws.String("v2"),
		ClientRequestToken: aws.String("token-v2-0000000000000000000000000"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(again.VersionId) != aws.ToString(put.VersionId) {
		t.Fatalf("idempotent put returned %v", aws.ToString(again.VersionId))
	}
	_, err = client.PutSecretValue(ctx, &secretsmanager.PutSecretValueInput{
		SecretId:           aws.String("rotating"),
		SecretString:       aws.String("other"),
		ClientRequestToken: aws.String("token-v2-0000000000000000000000000"),
	})
	if apiCode(err) != "ResourceExistsException" {
		t.Fatalf("conflicting token err = %v", err)
	}

	prev, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{
		SecretId:     aws.String("rotating"),
		VersionStage: aws.String("AWSPREVIOUS"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(prev.SecretString) != "v1" || aws.ToString(prev.VersionId) != aws.ToString(created.VersionId) {
		t.Fatalf("previous = %+v", prev)
	}

	pending, err := client.PutSecretValue(ctx, &secretsmanager.PutSecretValueInput{
		SecretId:      aws.String("rotating"),
		SecretString:  aws.String("v3"),
		VersionStages: []string{"AWSPENDING"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.RotateSecret(ctx, &secretsmanager.RotateSecretInput{SecretId: aws.String("rotating")}); apiCode(err) != "InvalidRequestException" {
		t.Fatalf("rotate with pending err = %v", err)
	}
	if _, err := client.UpdateSecretVersionStage(ctx, &secretsmanager.UpdateSecretVersionStageInput{
		SecretId:        aws.String("rotating"),
		VersionStage:    aws.String("AWSCURRENT"),
		MoveToVersionId: pending.VersionId,
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("move without remove err = %v", err)
	}
	if _, err := client.UpdateSecretVersionStage(ctx, &secretsmanager.UpdateSecretVersionStageInput{
		SecretId:            aws.String("rotating"),
		VersionStage:        aws.String("AWSCURRENT"),
		MoveToVersionId:     pending.VersionId,
		RemoveFromVersionId: put.VersionId,
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UpdateSecretVersionStage(ctx, &secretsmanager.UpdateSecretVersionStageInput{
		SecretId:            aws.String("rotating"),
		VersionStage:        aws.String("AWSPENDING"),
		RemoveFromVersionId: pending.VersionId,
	}); err != nil {
		t.Fatal(err)
	}

	listed, err := client.ListSecretVersionIds(ctx, &secretsmanager.ListSecretVersionIdsInput{SecretId: aws.String("rotating")})
	if err != nil {
		t.Fatal(err)
	}
	stages := map[string][]string{}
	for _, v := range listed.Versions {
		stages[aws.ToString(v.VersionId)] = v.VersionStages
	}
	want := map[string][]string{
		aws.ToString(put.VersionId):     {"AWSPREVIOUS"},
		aws.ToString(pending.VersionId): {"AWSCURRENT"},
	}
	if len(stages) != len(want) {
		t.Fatalf("versions = %v", stages)
	}
	for id, s := range want {
		if !slices.Equal(stages[id], s) {
			t.Fatalf("versions = %v", stages)
		}
	}
	deprecated, err := client.ListSecretVersionIds(ctx, &secretsmanager.ListSecretVersionIdsInput{
		SecretId:          aws.String("rotating"),
		IncludeDeprecated: aws.Bool(true),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(deprecated.Versions) != 3 {
		t.Fatalf("with deprecated = %d", len(deprecated.Versions))
	}

	byID, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("rotating"), VersionId: created.VersionId})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(byID.SecretString) != "v1" || len(byID.VersionStages) != 0 {
		t.Fatalf("deprecated version = %+v", byID)
	}
}

func TestSecretsManagerRotation(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	created := mustSecret(t, client, "rot", "value")

	rotated, err := client.RotateSecret(ctx, &secretsmanager.RotateSecretInput{
		SecretId:          aws.String("rot"),
		RotationLambdaARN: aws.String("arn:aws:lambda:us-east-1:000000000000:function:rotate"),
		RotationRules:     &types.RotationRulesType{AutomaticallyAfterDays: aws.Int64(30)},
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(rotated.VersionId) == aws.ToString(created.VersionId) {
		t.Fatal("rotation reused the version")
	}
	cur, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("rot")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(cur.VersionId) != aws.ToString(rotated.VersionId) || aws.ToString(cur.SecretString) != "value" {
		t.Fatalf("current after rotation = %+v", cur)
	}
	desc, err := client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("rot")})
	if err != nil {
		t.Fatal(err)
	}
	if !aws.ToBool(desc.RotationEnabled) || desc.LastRotatedDate == nil || desc.NextRotationDate == nil ||
		aws.ToInt64(desc.RotationRules.AutomaticallyAfterDays) != 30 ||
		aws.ToString(desc.RotationLambdaARN) != "arn:aws:lambda:us-east-1:000000000000:function:rotate" {
		t.Fatalf("describe after rotation = %+v", desc)
	}
	if d := desc.NextRotationDate.Sub(*desc.LastRotatedDate); d != 30*24*time.Hour {
		t.Fatalf("next rotation offset = %v", d)
	}
	if _, err := client.RotateSecret(ctx, &secretsmanager.RotateSecretInput{
		SecretId:      aws.String("rot"),
		RotationRules: &types.RotationRulesType{AutomaticallyAfterDays: aws.Int64(0)},
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("invalid rules err = %v", err)
	}

	if _, err := client.CancelRotateSecret(ctx, &secretsmanager.CancelRotateSecretInput{SecretId: aws.String("rot")}); err != nil {
		t.Fatal(err)
	}
	desc, err = client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("rot")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToBool(desc.RotationEnabled) || desc.NextRotationDate != nil {
		t.Fatalf("describe after cancel = %+v", desc)
	}
}

func TestSecretsManagerDeleteRestore(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	mustSecret(t, client, "doomed", "x")

	if _, err := client.DeleteSecret(ctx, &secretsmanager.DeleteSecretInput{SecretId: aws.String("doomed"), RecoveryWindowInDays: aws.Int64(6)}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("short window err = %v", err)
	}
	if _, err := client.DeleteSecret(ctx, &secretsmanager.DeleteSecretInput{
		SecretId:                   aws.String("doomed"),
		RecoveryWindowInDays:       aws.Int64(7),
		ForceDeleteWithoutRecovery: aws.Bool(true),
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("window with force err = %v", err)
	}
	deleted, err := client.DeleteSecret(ctx, &secretsmanager.DeleteSecretInput{SecretId: aws.String("doomed"), RecoveryWindowInDays: aws.Int64(7)})
	if err != nil {
		t.Fatal(err)
	}
	if d := time.Until(*deleted.DeletionDate); d < 6*24*time.Hour || d > 8*24*time.Hour {
		t.Fatalf("deletion date = %v", deleted.DeletionDate)
	}
	if _, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("doomed")}); apiCode(err) != "InvalidRequestException" {
		t.Fatalf("get deleted err = %v", err)
	}
	if _, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{Name: aws.String("doomed")}); apiCode(err) != "InvalidRequestException" {
		t.Fatalf("recreate scheduled err = %v", err)
	}
	listed, err := client.ListSecrets(ctx, &secretsmanager.ListSecretsInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.SecretList) != 0 {
		t.Fatalf("listed deleted secret: %+v", listed.SecretList)
	}
	listed, err = client.ListSecrets(ctx, &secretsmanager.ListSecretsInput{IncludePlannedDeletion: aws.Bool(true)})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.SecretList) != 1 || listed.SecretList[0].DeletedDate == nil {
		t.Fatalf("planned deletion list = %+v", listed.SecretList)
	}

	if _, err := client.RestoreSecret(ctx, &secretsmanager.RestoreSecretInput{SecretId: aws.String("doomed")}); err != nil {
		t.Fatal(err)
	}
	got, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("doomed")})
	if err != nil || aws.ToString(got.SecretString) != "x" {
		t.Fatalf("restored value = %+v, %v", got, err)
	}
	if _, err := client.RestoreSecret(ctx, &secretsmanager.RestoreSecretInput{SecretId: aws.String("doomed")}); apiCode(err) != "InvalidRequestException" {
		t.Fatalf("restore active err = %v", err)
	}

	if _, err := client.DeleteSecret(ctx, &secretsmanager.DeleteSecretInput{SecretId: aws.String("doomed"), ForceDeleteWithoutRecovery: aws.Bool(true)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("doomed")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("describe force-deleted err = %v", err)
	}
	mustSecret(t, client, "doomed", "again")
}

func TestSecretsManagerErrors(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	mustSecret(t, client, "dup", "x")

	_, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{Name: aws.String("dup"), SecretString: aws.String("x")})
	if apiCode(err) != "ResourceExistsException" || httpStatus(err) != 400 {
		t.Fatalf("duplicate err = %v", err)
	}
	if _, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("missing")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("missing err = %v", err)
	}
	if _, err := client.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("dup"), VersionStage: aws.String("AWSPENDING")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("missing stage err = %v", err)
	}
	if _, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{
		Name:         aws.String("both"),
		SecretString: aws.String("x"),
		SecretBinary: []byte("y"),
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("both values err = %v", err)
	}
	if _, err := client.ListSecrets(ctx, &secretsmanager.ListSecretsInput{MaxResults: aws.Int32(101)}); apiCode(err) != "ValidationException" {
		t.Fatalf("max results err = %v", err)
	}
	if _, err := client.ListSecrets(ctx, &secretsmanager.ListSecretsInput{NextToken: aws.String("garbage")}); apiCode(err) != "InvalidNextTokenException" {
		t.Fatalf("next token err = %v", err)
	}

	token := "create-token-000000000000000000000000"
	first, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{Name: aws.String("idem"), SecretString: aws.String("v"), ClientRequestToken: aws.String(token)})
	if err != nil {
		t.Fatal(err)
	}
	second, err := client.CreateSecret(ctx, &secretsmanager.CreateSecretInput{Name: aws.String("idem"), SecretString: aws.String("v"), ClientRequestToken: aws.String(token)})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(first.ARN) != aws.ToString(second.ARN) || aws.ToString(second.VersionId) != token {
		t.Fatalf("idempotent create = %+v / %+v", first, second)
	}
	if _, err := client.UpdateSecret(ctx, &secretsmanager.UpdateSecretInput{SecretId: aws.String("idem"), SecretString: aws.String("w"), ClientRequestToken: aws.String(token)}); apiCode(err) != "ResourceExistsException" {
		t.Fatalf("update reusing token err = %v", err)
	}
}

func TestSecretsManagerRandomPassword(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)

	def, err := client.GetRandomPassword(ctx, &secretsmanager.GetRandomPasswordInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(aws.ToString(def.RandomPassword)) != 32 {
		t.Fatalf("default password = %q", aws.ToString(def.RandomPassword))
	}
	out, err := client.GetRandomPassword(ctx, &secretsmanager.GetRandomPasswordInput{
		PasswordLength:     aws.Int64(64),
		ExcludePunctuation: aws.Bool(true),
		ExcludeCharacters:  aws.String("aeiouAEIOU01"),
	})
	if err != nil {
		t.Fatal(err)
	}
	p := aws.ToString(out.RandomPassword)
	if len(p) != 64 || strings.ContainsAny(p, "aeiouAEIOU01!@#$%") {
		t.Fatalf("password = %q", p)
	}
	other, err := client.GetRandomPassword(ctx, &secretsmanager.GetRandomPasswordInput{PasswordLength: aws.Int64(64), ExcludePunctuation: aws.Bool(true)})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(other.RandomPassword) == p {
		t.Fatal("two passwords were equal")
	}
	if _, err := client.GetRandomPassword(ctx, &secretsmanager.GetRandomPasswordInput{
		ExcludeLowercase:   aws.Bool(true),
		ExcludeUppercase:   aws.Bool(true),
		ExcludeNumbers:     aws.Bool(true),
		ExcludePunctuation: aws.Bool(true),
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("empty charset err = %v", err)
	}
}

func TestSecretsManagerBatchGet(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	a := mustSecret(t, client, "batch/a", "A")
	mustSecret(t, client, "batch/b", "B")
	mustSecret(t, client, "other", "O")

	out, err := client.BatchGetSecretValue(ctx, &secretsmanager.BatchGetSecretValueInput{
		SecretIdList: []string{aws.ToString(a.ARN), "batch/b", "missing"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.SecretValues) != 2 || aws.ToString(out.SecretValues[0].SecretString) != "A" || aws.ToString(out.SecretValues[1].SecretString) != "B" {
		t.Fatalf("values = %+v", out.SecretValues)
	}
	if len(out.Errors) != 1 || aws.ToString(out.Errors[0].SecretId) != "missing" || aws.ToString(out.Errors[0].ErrorCode) != "ResourceNotFoundException" {
		t.Fatalf("errors = %+v", out.Errors)
	}

	filtered, err := client.BatchGetSecretValue(ctx, &secretsmanager.BatchGetSecretValueInput{
		Filters: []types.Filter{{Key: types.FilterNameStringTypeName, Values: []string{"batch/"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(filtered.SecretValues) != 2 {
		t.Fatalf("filtered = %+v", filtered.SecretValues)
	}
	if _, err := client.BatchGetSecretValue(ctx, &secretsmanager.BatchGetSecretValueInput{
		SecretIdList: []string{"batch/a"},
		Filters:      []types.Filter{{Key: types.FilterNameStringTypeName, Values: []string{"batch/"}}},
	}); apiCode(err) != "InvalidParameterException" {
		t.Fatalf("both selectors err = %v", err)
	}
}

func TestSecretsManagerTags(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	mustSecret(t, client, "tagged", "x")

	if _, err := client.TagResource(ctx, &secretsmanager.TagResourceInput{
		SecretId: aws.String("tagged"),
		Tags: []types.Tag{
			{Key: aws.String("a"), Value: aws.String("1")},
			{Key: aws.String("b"), Value: aws.String("2")},
		},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.TagResource(ctx, &secretsmanager.TagResourceInput{
		SecretId: aws.String("tagged"),
		Tags:     []types.Tag{{Key: aws.String("a"), Value: aws.String("3")}},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UntagResource(ctx, &secretsmanager.UntagResourceInput{SecretId: aws.String("tagged"), TagKeys: []string{"b"}}); err != nil {
		t.Fatal(err)
	}
	desc, err := client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("tagged")})
	if err != nil {
		t.Fatal(err)
	}
	if len(desc.Tags) != 1 || aws.ToString(desc.Tags[0].Key) != "a" || aws.ToString(desc.Tags[0].Value) != "3" {
		t.Fatalf("tags = %+v", desc.Tags)
	}
	if _, err := client.TagResource(ctx, &secretsmanager.TagResourceInput{SecretId: aws.String("missing"), Tags: []types.Tag{}}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("tag missing err = %v", err)
	}
}

func TestSecretsManagerResourcePolicy(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	mustSecret(t, client, "guarded", "x")
	policy := `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"AWS":"arn:aws:iam::000000000000:root"},"Action":"secretsmanager:GetSecretValue","Resource":"*"}]}`

	if _, err := client.PutResourcePolicy(ctx, &secretsmanager.PutResourcePolicyInput{SecretId: aws.String("guarded"), ResourcePolicy: aws.String(policy)}); err != nil {
		t.Fatal(err)
	}
	got, err := client.GetResourcePolicy(ctx, &secretsmanager.GetResourcePolicyInput{SecretId: aws.String("guarded")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.ResourcePolicy) != policy {
		t.Fatalf("policy = %q", aws.ToString(got.ResourcePolicy))
	}
	valid, err := client.ValidateResourcePolicy(ctx, &secretsmanager.ValidateResourcePolicyInput{ResourcePolicy: aws.String(policy)})
	if err != nil {
		t.Fatal(err)
	}
	if !valid.PolicyValidationPassed {
		t.Fatal("validation failed")
	}
	if _, err := client.PutResourcePolicy(ctx, &secretsmanager.PutResourcePolicyInput{SecretId: aws.String("guarded"), ResourcePolicy: aws.String("{not json")}); apiCode(err) != "MalformedPolicyDocumentException" {
		t.Fatalf("malformed err = %v", err)
	}
	public := `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":"*","Action":"secretsmanager:GetSecretValue","Resource":"*"}]}`
	if _, err := client.PutResourcePolicy(ctx, &secretsmanager.PutResourcePolicyInput{
		SecretId:          aws.String("guarded"),
		ResourcePolicy:    aws.String(public),
		BlockPublicPolicy: aws.Bool(true),
	}); apiCode(err) != "PublicPolicyException" {
		t.Fatalf("public policy err = %v", err)
	}
	if _, err := client.DeleteResourcePolicy(ctx, &secretsmanager.DeleteResourcePolicyInput{SecretId: aws.String("guarded")}); err != nil {
		t.Fatal(err)
	}
	got, err = client.GetResourcePolicy(ctx, &secretsmanager.GetResourcePolicyInput{SecretId: aws.String("guarded")})
	if err != nil {
		t.Fatal(err)
	}
	if got.ResourcePolicy != nil {
		t.Fatalf("policy after delete = %q", aws.ToString(got.ResourcePolicy))
	}
}

func TestSecretsManagerReplication(t *testing.T) {
	ctx := context.Background()
	client := smClient(t)
	mustSecret(t, client, "replicated", "x")

	out, err := client.ReplicateSecretToRegions(ctx, &secretsmanager.ReplicateSecretToRegionsInput{
		SecretId:          aws.String("replicated"),
		AddReplicaRegions: []types.ReplicaRegionType{{Region: aws.String("us-west-2")}, {Region: aws.String("eu-west-1")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.ReplicationStatus) != 2 || out.ReplicationStatus[0].Status != types.StatusTypeInSync {
		t.Fatalf("replication = %+v", out.ReplicationStatus)
	}
	removed, err := client.RemoveRegionsFromReplication(ctx, &secretsmanager.RemoveRegionsFromReplicationInput{
		SecretId:             aws.String("replicated"),
		RemoveReplicaRegions: []string{"eu-west-1"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(removed.ReplicationStatus) != 1 || aws.ToString(removed.ReplicationStatus[0].Region) != "us-west-2" {
		t.Fatalf("after remove = %+v", removed.ReplicationStatus)
	}
	desc, err := client.DescribeSecret(ctx, &secretsmanager.DescribeSecretInput{SecretId: aws.String("replicated")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(desc.PrimaryRegion) != "us-east-1" || len(desc.ReplicationStatus) != 1 {
		t.Fatalf("describe = %+v", desc)
	}
	if _, err := client.StopReplicationToReplica(ctx, &secretsmanager.StopReplicationToReplicaInput{SecretId: aws.String("replicated")}); err != nil {
		t.Fatal(err)
	}
}

func TestSecretsManagerIsolation(t *testing.T) {
	ctx := context.Background()
	a := smClient(t)
	b := smClient(t)
	mustSecret(t, a, "shared-name", "from-a")

	if _, err := b.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("shared-name")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("other mock err = %v", err)
	}
	mustSecret(t, b, "shared-name", "from-b")
	got, err := a.GetSecretValue(ctx, &secretsmanager.GetSecretValueInput{SecretId: aws.String("shared-name")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.SecretString) != "from-a" {
		t.Fatalf("value = %q", aws.ToString(got.SecretString))
	}
}
