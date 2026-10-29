package wasmo_test

import (
	"context"
	"fmt"
	"slices"
	"sort"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ssm"
	"github.com/aws/aws-sdk-go-v2/service/ssm/types"
)

func newSSM(t *testing.T) *ssm.Client {
	t.Helper()
	return ssm.NewFromConfig(newMock(t).Config())
}

func putParam(t *testing.T, client *ssm.Client, name, value string, typ types.ParameterType) int64 {
	t.Helper()
	out, err := client.PutParameter(context.Background(), &ssm.PutParameterInput{
		Name:      aws.String(name),
		Value:     aws.String(value),
		Type:      typ,
		Overwrite: aws.Bool(true),
	})
	if err != nil {
		t.Fatalf("put %s: %v", name, err)
	}
	return out.Version
}

func paramNames(params []types.Parameter) []string {
	names := make([]string, len(params))
	for i, p := range params {
		names[i] = aws.ToString(p.Name)
	}
	sort.Strings(names)
	return names
}

func TestSSMPutGetVersions(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)

	first, err := client.PutParameter(ctx, &ssm.PutParameterInput{
		Name:        aws.String("/app/db/host"),
		Value:       aws.String("db1"),
		Type:        types.ParameterTypeString,
		Description: aws.String("database host"),
		Tags:        []types.Tag{{Key: aws.String("env"), Value: aws.String("dev")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if first.Version != 1 || first.Tier != types.ParameterTierStandard {
		t.Fatalf("put = %+v", first)
	}

	_, err = client.PutParameter(ctx, &ssm.PutParameterInput{Name: aws.String("/app/db/host"), Value: aws.String("db2"), Type: types.ParameterTypeString})
	if apiCode(err) != "ParameterAlreadyExists" {
		t.Fatalf("duplicate put err = %v", err)
	}
	if v := putParam(t, client, "/app/db/host", "db2", ""); v != 2 {
		t.Fatalf("overwrite version = %d", v)
	}

	got, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/db/host")})
	if err != nil {
		t.Fatal(err)
	}
	p := got.Parameter
	if aws.ToString(p.Value) != "db2" || p.Version != 2 || p.Type != types.ParameterTypeString || aws.ToString(p.DataType) != "text" {
		t.Fatalf("parameter = %+v", p)
	}
	if aws.ToString(p.ARN) != "arn:aws:ssm:us-east-1:000000000000:parameter/app/db/host" || p.LastModifiedDate == nil {
		t.Fatalf("arn/date = %v %v", aws.ToString(p.ARN), p.LastModifiedDate)
	}

	old, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/db/host:1")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(old.Parameter.Value) != "db1" || aws.ToString(old.Parameter.Selector) != ":1" || aws.ToString(old.Parameter.Name) != "/app/db/host" {
		t.Fatalf("selector parameter = %+v", old.Parameter)
	}

	byArn, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: p.ARN})
	if err != nil || aws.ToString(byArn.Parameter.Value) != "db2" {
		t.Fatalf("by arn = %+v, %v", byArn, err)
	}

	_, err = client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/db/host:9")})
	if apiCode(err) != "ParameterVersionNotFound" {
		t.Fatalf("missing version err = %v", err)
	}
	_, err = client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/missing")})
	if apiCode(err) != "ParameterNotFound" || httpStatus(err) != 400 {
		t.Fatalf("missing parameter err = %v", err)
	}

	many, err := client.GetParameters(ctx, &ssm.GetParametersInput{Names: []string{"/app/db/host", "/app/db/host:1", "/app/missing", "/app/db/host:7"}})
	if err != nil {
		t.Fatal(err)
	}
	if len(many.Parameters) != 2 || !slices.Equal(many.InvalidParameters, []string{"/app/missing", "/app/db/host:7"}) {
		t.Fatalf("get parameters = %+v", many)
	}
}

func TestSSMSecureString(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)

	if _, err := client.PutParameter(ctx, &ssm.PutParameterInput{
		Name:  aws.String("/app/secret"),
		Value: aws.String("hunter2"),
		Type:  types.ParameterTypeSecureString,
		KeyId: aws.String("alias/app"),
	}); err != nil {
		t.Fatal(err)
	}
	plain, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/secret"), WithDecryption: aws.Bool(true)})
	if err != nil || aws.ToString(plain.Parameter.Value) != "hunter2" {
		t.Fatalf("decrypted = %+v, %v", plain, err)
	}
	cipher, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/app/secret")})
	if err != nil {
		t.Fatal(err)
	}
	if v := aws.ToString(cipher.Parameter.Value); v == "hunter2" || strings.Contains(v, "hunter2") || v == "" {
		t.Fatalf("encrypted value = %q", v)
	}
	desc, err := client.DescribeParameters(ctx, &ssm.DescribeParametersInput{})
	if err != nil || len(desc.Parameters) != 1 || aws.ToString(desc.Parameters[0].KeyId) != "alias/app" {
		t.Fatalf("describe = %+v, %v", desc, err)
	}

	_, err = client.PutParameter(ctx, &ssm.PutParameterInput{Name: aws.String("/app/bad-key"), Value: aws.String("x"), Type: types.ParameterTypeSecureString, KeyId: aws.String("not a key")})
	if apiCode(err) != "InvalidKeyId" {
		t.Fatalf("bad key err = %v", err)
	}
}

func TestSSMValidation(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	put := func(in *ssm.PutParameterInput) error {
		if in.Value == nil {
			in.Value = aws.String("v")
		}
		if in.Type == "" {
			in.Type = types.ParameterTypeString
		}
		_, err := client.PutParameter(ctx, in)
		return err
	}
	deep := "/" + strings.Repeat("l/", 15) + "x"
	cases := []struct {
		name string
		in   *ssm.PutParameterInput
		code string
	}{
		{"relative path", &ssm.PutParameterInput{Name: aws.String("a/b")}, "ValidationException"},
		{"reserved prefix", &ssm.PutParameterInput{Name: aws.String("/aws/thing")}, "ValidationException"},
		{"too deep", &ssm.PutParameterInput{Name: aws.String(deep)}, "HierarchyLevelLimitExceededException"},
		{"pattern mismatch", &ssm.PutParameterInput{Name: aws.String("/n"), Value: aws.String("12a"), AllowedPattern: aws.String(`^\d+$`)}, "ParameterPatternMismatchException"},
		{"bad pattern", &ssm.PutParameterInput{Name: aws.String("/n"), AllowedPattern: aws.String("([")}, "InvalidAllowedPatternException"},
		{"bad data type", &ssm.PutParameterInput{Name: aws.String("/n"), DataType: aws.String("Text")}, "ValidationException"},
		{"bad ami", &ssm.PutParameterInput{Name: aws.String("/n"), DataType: aws.String("aws:ec2:image")}, "ValidationException"},
		{"standard too large", &ssm.PutParameterInput{Name: aws.String("/n"), Value: aws.String(strings.Repeat("x", 5000))}, "ValidationException"},
	}
	for _, c := range cases {
		if code := apiCode(put(c.in)); code != c.code {
			t.Errorf("%s: code = %q, want %q", c.name, code, c.code)
		}
	}

	if err := put(&ssm.PutParameterInput{Name: aws.String("/n"), Value: aws.String("123"), AllowedPattern: aws.String(`^\d+$`)}); err != nil {
		t.Fatal(err)
	}
	if code := apiCode(put(&ssm.PutParameterInput{Name: aws.String("/n"), Value: aws.String("abc"), Overwrite: aws.Bool(true)})); code != "ParameterPatternMismatchException" {
		t.Fatalf("inherited pattern code = %q", code)
	}
	if code := apiCode(put(&ssm.PutParameterInput{Name: aws.String("/n"), Overwrite: aws.Bool(true), Tags: []types.Tag{{Key: aws.String("k"), Value: aws.String("v")}}})); code != "ValidationException" {
		t.Fatalf("tags with overwrite code = %q", code)
	}
	if err := put(&ssm.PutParameterInput{Name: aws.String("/ami"), Value: aws.String("ami-0123456789abcdef0"), DataType: aws.String("aws:ec2:image")}); err != nil {
		t.Fatal(err)
	}

	big, err := client.PutParameter(ctx, &ssm.PutParameterInput{Name: aws.String("/big"), Value: aws.String(strings.Repeat("x", 5000)), Type: types.ParameterTypeString, Tier: types.ParameterTierIntelligentTiering})
	if err != nil || big.Tier != types.ParameterTierAdvanced {
		t.Fatalf("intelligent tiering = %+v, %v", big, err)
	}
	if code := apiCode(put(&ssm.PutParameterInput{Name: aws.String("/big"), Overwrite: aws.Bool(true), Tier: types.ParameterTierStandard})); code != "ValidationException" {
		t.Fatalf("downgrade code = %q", code)
	}
}

func TestSSMLabels(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	for _, v := range []string{"v1", "v2", "v3"} {
		putParam(t, client, "/cfg/flag", v, types.ParameterTypeString)
	}

	out, err := client.LabelParameterVersion(ctx, &ssm.LabelParameterVersionInput{
		Name:             aws.String("/cfg/flag"),
		ParameterVersion: aws.Int64(1),
		Labels:           []string{"stable", "1bad", "awsx", "canary"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if out.ParameterVersion != 1 || !slices.Equal(out.InvalidLabels, []string{"1bad", "awsx"}) {
		t.Fatalf("label = %+v", out)
	}
	got, err := client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/cfg/flag:stable")})
	if err != nil || aws.ToString(got.Parameter.Value) != "v1" || aws.ToString(got.Parameter.Selector) != ":stable" {
		t.Fatalf("by label = %+v, %v", got, err)
	}

	latest, err := client.LabelParameterVersion(ctx, &ssm.LabelParameterVersionInput{Name: aws.String("/cfg/flag"), Labels: []string{"stable"}})
	if err != nil || latest.ParameterVersion != 3 {
		t.Fatalf("move label = %+v, %v", latest, err)
	}
	got, err = client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/cfg/flag:stable")})
	if err != nil || aws.ToString(got.Parameter.Value) != "v3" {
		t.Fatalf("moved label = %+v, %v", got, err)
	}

	hist, err := client.GetParameterHistory(ctx, &ssm.GetParameterHistoryInput{Name: aws.String("/cfg/flag")})
	if err != nil {
		t.Fatal(err)
	}
	if len(hist.Parameters) != 3 || !slices.Equal(hist.Parameters[0].Labels, []string{"canary"}) || !slices.Equal(hist.Parameters[2].Labels, []string{"stable"}) {
		t.Fatalf("history = %+v", hist.Parameters)
	}

	_, err = client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/cfg/flag:nolabel")})
	if apiCode(err) != "ParameterNotFound" {
		t.Fatalf("unknown label err = %v", err)
	}
	_, err = client.LabelParameterVersion(ctx, &ssm.LabelParameterVersionInput{Name: aws.String("/cfg/flag"), ParameterVersion: aws.Int64(9), Labels: []string{"x"}})
	if apiCode(err) != "ParameterVersionNotFound" {
		t.Fatalf("label missing version err = %v", err)
	}
	labels := make([]string, 10)
	for i := range labels {
		labels[i] = fmt.Sprintf("l%d", i)
	}
	_, err = client.LabelParameterVersion(ctx, &ssm.LabelParameterVersionInput{Name: aws.String("/cfg/flag"), ParameterVersion: aws.Int64(1), Labels: labels})
	if apiCode(err) != "ParameterVersionLabelLimitExceeded" {
		t.Fatalf("label limit err = %v", err)
	}

	un, err := client.UnlabelParameterVersion(ctx, &ssm.UnlabelParameterVersionInput{Name: aws.String("/cfg/flag"), ParameterVersion: aws.Int64(3), Labels: []string{"stable", "canary"}})
	if err != nil || !slices.Equal(un.RemovedLabels, []string{"stable"}) || !slices.Equal(un.InvalidLabels, []string{"canary"}) {
		t.Fatalf("unlabel = %+v, %v", un, err)
	}
}

func TestSSMVersionLimit(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	for i := range 100 {
		putParam(t, client, "/limit", fmt.Sprint(i), types.ParameterTypeString)
	}
	if _, err := client.LabelParameterVersion(ctx, &ssm.LabelParameterVersionInput{Name: aws.String("/limit"), ParameterVersion: aws.Int64(1), Labels: []string{"pin"}}); err != nil {
		t.Fatal(err)
	}
	_, err := client.PutParameter(ctx, &ssm.PutParameterInput{Name: aws.String("/limit"), Value: aws.String("x"), Overwrite: aws.Bool(true)})
	if apiCode(err) != "ParameterMaxVersionLimitExceeded" {
		t.Fatalf("limit err = %v", err)
	}
	if _, err := client.UnlabelParameterVersion(ctx, &ssm.UnlabelParameterVersionInput{Name: aws.String("/limit"), ParameterVersion: aws.Int64(1), Labels: []string{"pin"}}); err != nil {
		t.Fatal(err)
	}
	if v := putParam(t, client, "/limit", "x", ""); v != 101 {
		t.Fatalf("version = %d", v)
	}
	_, err = client.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/limit:1")})
	if apiCode(err) != "ParameterVersionNotFound" {
		t.Fatalf("dropped version err = %v", err)
	}
	var count int
	pages := ssm.NewGetParameterHistoryPaginator(client, &ssm.GetParameterHistoryInput{Name: aws.String("/limit")})
	for pages.HasMorePages() {
		page, err := pages.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		count += len(page.Parameters)
	}
	if count != 100 {
		t.Fatalf("history count = %d", count)
	}
}

func TestSSMGetParametersByPath(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	for _, name := range []string{"/svc/a", "/svc/b", "/svc/nested/c", "/svc/nested/deep/d", "/svcx/e", "flat"} {
		putParam(t, client, name, name, types.ParameterTypeString)
	}
	putParam(t, client, "/svc/secret", "s", types.ParameterTypeSecureString)

	one, err := client.GetParametersByPath(ctx, &ssm.GetParametersByPathInput{Path: aws.String("/svc")})
	if err != nil {
		t.Fatal(err)
	}
	if got := paramNames(one.Parameters); !slices.Equal(got, []string{"/svc/a", "/svc/b", "/svc/secret"}) {
		t.Fatalf("one level = %v", got)
	}

	var all []types.Parameter
	pages := ssm.NewGetParametersByPathPaginator(client, &ssm.GetParametersByPathInput{
		Path:           aws.String("/svc/"),
		Recursive:      aws.Bool(true),
		WithDecryption: aws.Bool(true),
		MaxResults:     aws.Int32(2),
	})
	var pageCount int
	for pages.HasMorePages() {
		page, err := pages.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pageCount++
		all = append(all, page.Parameters...)
	}
	if got := paramNames(all); pageCount != 3 || !slices.Equal(got, []string{"/svc/a", "/svc/b", "/svc/nested/c", "/svc/nested/deep/d", "/svc/secret"}) {
		t.Fatalf("recursive (%d pages) = %v", pageCount, got)
	}
	for _, p := range all {
		if aws.ToString(p.Name) == "/svc/secret" && aws.ToString(p.Value) != "s" {
			t.Fatalf("secret not decrypted: %q", aws.ToString(p.Value))
		}
	}

	secure, err := client.GetParametersByPath(ctx, &ssm.GetParametersByPathInput{
		Path:             aws.String("/svc"),
		Recursive:        aws.Bool(true),
		ParameterFilters: []types.ParameterStringFilter{{Key: aws.String("Type"), Values: []string{"SecureString"}}},
	})
	if err != nil || !slices.Equal(paramNames(secure.Parameters), []string{"/svc/secret"}) {
		t.Fatalf("type filter = %+v, %v", secure, err)
	}

	_, err = client.GetParametersByPath(ctx, &ssm.GetParametersByPathInput{
		Path:             aws.String("/svc"),
		ParameterFilters: []types.ParameterStringFilter{{Key: aws.String("Name"), Values: []string{"/svc/a"}}},
	})
	if apiCode(err) != "InvalidFilterKey" {
		t.Fatalf("name filter by path err = %v", err)
	}
	_, err = client.GetParametersByPath(ctx, &ssm.GetParametersByPathInput{Path: aws.String("/svc"), NextToken: aws.String("garbage")})
	if apiCode(err) != "InvalidNextToken" {
		t.Fatalf("bad token err = %v", err)
	}
	_, err = client.GetParametersByPath(ctx, &ssm.GetParametersByPathInput{Path: aws.String("svc")})
	if apiCode(err) != "ValidationException" {
		t.Fatalf("relative path err = %v", err)
	}
}

func TestSSMDescribeParameters(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	putParam(t, client, "/team/a", "1", types.ParameterTypeString)
	putParam(t, client, "/team/b", "2", types.ParameterTypeStringList)
	putParam(t, client, "/team/sub/c", "3", types.ParameterTypeSecureString)
	putParam(t, client, "other", "4", types.ParameterTypeString)
	if _, err := client.AddTagsToResource(ctx, &ssm.AddTagsToResourceInput{
		ResourceType: types.ResourceTypeForTaggingParameter,
		ResourceId:   aws.String("/team/b"),
		Tags:         []types.Tag{{Key: aws.String("owner"), Value: aws.String("alice")}},
	}); err != nil {
		t.Fatal(err)
	}

	describe := func(filters ...types.ParameterStringFilter) []string {
		t.Helper()
		var names []string
		pages := ssm.NewDescribeParametersPaginator(client, &ssm.DescribeParametersInput{ParameterFilters: filters, MaxResults: aws.Int32(1)})
		for pages.HasMorePages() {
			page, err := pages.NextPage(ctx)
			if err != nil {
				t.Fatal(err)
			}
			for _, p := range page.Parameters {
				names = append(names, aws.ToString(p.Name))
			}
		}
		sort.Strings(names)
		return names
	}
	filter := func(key, option string, values ...string) types.ParameterStringFilter {
		f := types.ParameterStringFilter{Key: aws.String(key), Values: values}
		if option != "" {
			f.Option = aws.String(option)
		}
		return f
	}
	cases := []struct {
		name   string
		filter []types.ParameterStringFilter
		want   []string
	}{
		{"all", nil, []string{"/team/a", "/team/b", "/team/sub/c", "other"}},
		{"name equals", []types.ParameterStringFilter{filter("Name", "", "team/a")}, []string{"/team/a"}},
		{"name begins", []types.ParameterStringFilter{filter("Name", "BeginsWith", "/team/")}, []string{"/team/a", "/team/b", "/team/sub/c"}},
		{"name contains", []types.ParameterStringFilter{filter("Name", "Contains", "th")}, []string{"other"}},
		{"path one level", []types.ParameterStringFilter{filter("Path", "", "/team")}, []string{"/team/a", "/team/b"}},
		{"path recursive", []types.ParameterStringFilter{filter("Path", "Recursive", "/team")}, []string{"/team/a", "/team/b", "/team/sub/c"}},
		{"type", []types.ParameterStringFilter{filter("Type", "", "StringList", "SecureString")}, []string{"/team/b", "/team/sub/c"}},
		{"key id", []types.ParameterStringFilter{filter("KeyId", "", "alias/aws/ssm")}, []string{"/team/sub/c"}},
		{"tier", []types.ParameterStringFilter{filter("Tier", "", "Advanced")}, nil},
		{"tag", []types.ParameterStringFilter{filter("tag:owner", "")}, []string{"/team/b"}},
		{"tag value", []types.ParameterStringFilter{filter("tag:owner", "", "bob")}, nil},
		{"combined", []types.ParameterStringFilter{filter("Path", "Recursive", "/team"), filter("Type", "", "String")}, []string{"/team/a"}},
	}
	for _, c := range cases {
		if got := describe(c.filter...); !slices.Equal(got, c.want) {
			t.Errorf("%s: %v, want %v", c.name, got, c.want)
		}
	}

	legacy, err := client.DescribeParameters(ctx, &ssm.DescribeParametersInput{Filters: []types.ParametersFilter{{Key: types.ParametersFilterKeyType, Values: []string{"String"}}}})
	if err != nil || len(legacy.Parameters) != 2 || legacy.Parameters[0].Tier != types.ParameterTierStandard || legacy.Parameters[0].LastModifiedDate == nil {
		t.Fatalf("legacy = %+v, %v", legacy, err)
	}
	for key, code := range map[string]string{"Label": "InvalidFilterKey", "Bogus": "InvalidFilterKey"} {
		_, err := client.DescribeParameters(ctx, &ssm.DescribeParametersInput{ParameterFilters: []types.ParameterStringFilter{filter(key, "", "x")}})
		if apiCode(err) != code {
			t.Errorf("%s err = %v", key, err)
		}
	}
	_, err = client.DescribeParameters(ctx, &ssm.DescribeParametersInput{ParameterFilters: []types.ParameterStringFilter{filter("Type", "Contains", "String")}})
	if apiCode(err) != "InvalidFilterOption" {
		t.Fatalf("option err = %v", err)
	}
}

func TestSSMTagsAndDelete(t *testing.T) {
	ctx := context.Background()
	client := newSSM(t)
	if _, err := client.PutParameter(ctx, &ssm.PutParameterInput{
		Name:  aws.String("/t/one"),
		Value: aws.String("1"),
		Type:  types.ParameterTypeString,
		Tags:  []types.Tag{{Key: aws.String("b"), Value: aws.String("2")}},
	}); err != nil {
		t.Fatal(err)
	}
	putParam(t, client, "/t/two", "2", types.ParameterTypeString)

	ref := &ssm.ListTagsForResourceInput{ResourceType: types.ResourceTypeForTaggingParameter, ResourceId: aws.String("/t/one")}
	if _, err := client.AddTagsToResource(ctx, &ssm.AddTagsToResourceInput{
		ResourceType: types.ResourceTypeForTaggingParameter,
		ResourceId:   aws.String("/t/one"),
		Tags:         []types.Tag{{Key: aws.String("a"), Value: aws.String("1")}, {Key: aws.String("b"), Value: aws.String("3")}},
	}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForResource(ctx, ref)
	if err != nil || len(tags.TagList) != 2 || aws.ToString(tags.TagList[0].Key) != "b" || aws.ToString(tags.TagList[0].Value) != "3" || aws.ToString(tags.TagList[1].Key) != "a" {
		t.Fatalf("tags = %+v, %v", tags, err)
	}
	if _, err := client.RemoveTagsFromResource(ctx, &ssm.RemoveTagsFromResourceInput{ResourceType: types.ResourceTypeForTaggingParameter, ResourceId: aws.String("/t/one"), TagKeys: []string{"a"}}); err != nil {
		t.Fatal(err)
	}
	tags, err = client.ListTagsForResource(ctx, ref)
	if err != nil || len(tags.TagList) != 1 {
		t.Fatalf("tags after remove = %+v, %v", tags, err)
	}
	_, err = client.ListTagsForResource(ctx, &ssm.ListTagsForResourceInput{ResourceType: types.ResourceTypeForTaggingParameter, ResourceId: aws.String("/t/none")})
	if apiCode(err) != "InvalidResourceId" {
		t.Fatalf("missing resource err = %v", err)
	}

	if _, err := client.DeleteParameter(ctx, &ssm.DeleteParameterInput{Name: aws.String("/t/one")}); err != nil {
		t.Fatal(err)
	}
	_, err = client.DeleteParameter(ctx, &ssm.DeleteParameterInput{Name: aws.String("/t/one")})
	if apiCode(err) != "ParameterNotFound" {
		t.Fatalf("delete twice err = %v", err)
	}
	if v := putParam(t, client, "/t/one", "again", types.ParameterTypeString); v != 1 {
		t.Fatalf("recreated version = %d", v)
	}
	tags, err = client.ListTagsForResource(ctx, ref)
	if err != nil || len(tags.TagList) != 0 {
		t.Fatalf("tags survived delete = %+v, %v", tags, err)
	}

	del, err := client.DeleteParameters(ctx, &ssm.DeleteParametersInput{Names: []string{"/t/one", "/t/two", "/t/none"}})
	if err != nil || !slices.Equal(del.DeletedParameters, []string{"/t/one", "/t/two"}) || !slices.Equal(del.InvalidParameters, []string{"/t/none"}) {
		t.Fatalf("delete parameters = %+v, %v", del, err)
	}
}

func TestSSMIsolation(t *testing.T) {
	ctx := context.Background()
	a, b := newSSM(t), newSSM(t)
	putParam(t, a, "/shared/name", "a", types.ParameterTypeString)
	_, err := b.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/shared/name")})
	if apiCode(err) != "ParameterNotFound" {
		t.Fatalf("mock b sees mock a: %v", err)
	}
	if v := putParam(t, b, "/shared/name", "b", types.ParameterTypeString); v != 1 {
		t.Fatalf("mock b version = %d", v)
	}
	got, err := a.GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String("/shared/name")})
	if err != nil || aws.ToString(got.Parameter.Value) != "a" {
		t.Fatalf("mock a = %+v, %v", got, err)
	}
}
