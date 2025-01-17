use llvm_sys::LLVMIntPredicate;

use super::*;
use crate::{
    common::types::IntoOpaqueType,
    common::{
        types::{BasicType, ComplexType},
        *,
    },
    compiler::llvm::Insn,
    compiler_error, compiler_error_loc,
    func::{utils::FunctionCompilerUtils, *},
};

#[derive(Debug)]
pub struct ResolvedEnumMember {
    pub enum_impl: ResolvedEnumNode,
    pub variant_id: usize,
    pub data_type: Option<Vec<AnonymousStructField>>,
}

pub trait ExprCompiler {
    fn compile_expr(&mut self, expr: &Token<Expr>, type_hint: Option<&ComplexType>) -> Result<TypedValue>;
    fn compile_default_expr(&mut self, ty: &ComplexType) -> Result<TypedValue>;
    fn compile_reference_expr(&mut self, val: &TypedValue) -> Result<TypedValue>;
    fn compile_dereference_expr(&mut self, val: &TypedValue) -> Result<TypedValue>;
    fn compile_integer_literal_expr(&mut self, val: i64, type_hint: Option<&ComplexType>) -> Result<TypedValue>;
    fn compile_string_literal_expr(&mut self, str: &str) -> Result<TypedValue>;
    fn compile_sizeof_expr(&mut self, ty: &ComplexType) -> Result<TypedValue>;
    fn resolve_static_field_reference(&mut self, sfr: &Qualifier) -> Result<TypedValue>;
    fn resolve_enum_variant(&mut self, declaring_type: &GenericIdentifier, member: &str) -> Result<ResolvedEnumMember>;
    fn compile_new_enum_member(
        &mut self,
        declaring_type: &GenericIdentifier,
        member: &str,
        data: Option<Vec<NewCallField>>,
    ) -> Result<TypedValue>;
    fn compile_match_expr(&mut self, mtch: &MatchExpr) -> Result<TypedValue>;
    fn load_local_var(&mut self, var: &LocalVar) -> Result<TypedValue>;
}

impl<'a> ExprCompiler for FunctionCompiler<'a> {
    /// Returns a single scalar value loaded, denoted by the given AST expression node.
    /// For identifiers, field references, etc. the deferenced value is returned, not the pointer to the value.
    fn compile_expr(&mut self, expr: &Token<Expr>, type_hint: Option<&ComplexType>) -> Result<TypedValue> {
        self.loc(&expr.loc);
        Ok(match &expr.token {
            Expr::SignedIntLit(val) => self.compile_integer_literal_expr(*val, type_hint)?,
            Expr::StringLit(str) => self.compile_string_literal_expr(str)?,
            Expr::Ident(ident) => {
                let field_ref = self.resolve_static_field_reference(&Qualifier(vec![ident.clone()]))?;
                let container = TypedValueContainer(field_ref);
                TypedValue::new(container.get_type(), container.load(self)?)
            }
            Expr::Reference(expr) => {
                let compiled_expr = self.compile_expr(expr, None)?;
                self.compile_reference_expr(&compiled_expr)?
            }
            Expr::Dereference(expr) => {
                let compiled_expr = self.compile_expr(expr, None)?;
                self.compile_dereference_expr(&compiled_expr)?
            }
            Expr::FuncCall(func_call) => self.compile_static_func_call(&StaticFuncCall {
                owner: Qualifier(Vec::new()),
                call: func_call.clone(),
            })?,
            Expr::Member(member_expr) => {
                let mut members = member_expr.members.clone();

                let (mut current, skip) = if let Some(prefix) = &member_expr.prefix {
                    let (current, skip) = if prefix.0.len() == 1 && let Ok(local_ident) = self.resolve_ident(&prefix.0[0]) {
                        (self.load_local_var(&local_ident)?, 0)
                    } else {
                        let result = 'result: {
                            let imported_names =
                                utils::lookup_import_map(&self.import_map, &prefix.to_string());
                            match &members[0].value.token {
                                Expr::FuncCall(call) => {
                                    for imported_name in imported_names {
                                        let full_name =
                                            format!("{}::{}", imported_name, call.name.token.0);
                                        if self
                                            .cpl
                                            .type_provider
                                            .has_any_function_by_name(&full_name)
                                        {
                                            break 'result Some(self.compile_static_func_call(
                                                &StaticFuncCall {
                                                    owner: prefix.clone(),
                                                    call: call.clone(),
                                                },
                                            )?);
                                        }
                                    }
                                    break 'result None;
                                }
                                Expr::Ident(ident) => {
                                    for imported_name in imported_names {
                                        let mut parts = Vec::new();
                                        for part in imported_name.split("::") {
                                            parts.push(Token {
                                                token: Identifier(part.to_owned()),
                                                loc: prefix.get_location(),
                                            })
                                        }
                                        parts.push(ident.clone());

                                        if let Ok(sfr) =
                                            self.resolve_static_field_reference(&Qualifier(parts))
                                        {
                                            break 'result Some(sfr);
                                        }
                                    }
                                    break 'result None;
                                }
                                x => unreachable!("{:#?}", x),
                            }
                        };
                        match result {
                            Some(result) => (result, 1),
                            None => {
                                let identifier = self.resolve_static_field_reference(prefix)?;
                                (identifier, 0)
                            }
                        }
                    };
                    if skip == 0 {
                        members[0].ty = MemberType::Class;
                    }
                    (current, skip)
                } else {
                    (self.compile_expr(&members[0].value, None)?, 1)
                };

                for next_member in members.iter().skip(skip) {
                    self.loc(&next_member.value.loc);

                    current = self.unbox_object(current)?;

                    match (next_member.ty, current.ty.clone()) {
                        (MemberType::Class, ComplexType::Array(element_type)) => match &next_member.value.token {
                            Expr::Ident(field_name) => {
                                if field_name.token.0 == "length" {
                                    let length_ptr = self.emit(Insn::GetElementPtr(
                                        current.val,
                                        self.cpl.context.get_abi_slice_type(element_type.as_llvm_type(self.cpl), &element_type.to_string()),
                                        1,
                                    ));

                                    current = TypedValue {
                                        ty: BasicType::USize.to_complex(),
                                        val: self.emit(Insn::Load(length_ptr, BasicType::USize.as_llvm_type(self.cpl))),
                                    };
                                } else {
                                    return Err(compiler_error!(
                                        self,
                                        "No such field `{}` in type `{}`",
                                        field_name.token.0,
                                        current.ty.to_string()
                                    ));
                                }
                            }
                            x => unimplemented!("{:#?}", x),
                        },
                        (MemberType::Array, ComplexType::Array(_)) => {
                            let index = self.compile_expr(&next_member.value, Some(&BasicType::USize.to_complex()))?;
                            self.assert_assignable_to(&index.ty, &BasicType::USize.to_complex())?;
                            current = self.load_array_element(&current, &index)?;
                        }
                        (MemberType::Class, ComplexType::Basic(_)) => {
                            let instance = self.autobox_primitive(current.clone())?;
                            let ident = match &instance.ty {
                                ComplexType::Basic(BasicType::Object(ident)) => ident,
                                ty => return Err(compiler_error!(self, "The `.` operator is forbidden on type `{}`", ty.to_string())),
                            };

                            let class_impl = match self.cpl.type_provider.get_class_by_name(ident) {
                                Some(class_impl) => match class_impl.class_type {
                                    ClassType::Interface => {
                                        match &next_member.value.token {
                                            Expr::FuncCall(fc) => {
                                                current = self.compile_instance_func_call(fc, &instance)?;
                                            }
                                            Expr::Ident(field) => {
                                                return Err(compiler_error!(
                                                    self,
                                                    "Could not resolve accessor `{}::{}`",
                                                    ident.to_string(),
                                                    field.token.0
                                                ))
                                            }
                                            x => unreachable!("{:?}", x),
                                        }

                                        continue;
                                    }
                                    _ => class_impl,
                                },
                                None => match self.cpl.type_provider.get_enum_by_name(ident) {
                                    Some(_) => match &next_member.value.token {
                                        Expr::FuncCall(fc) => {
                                            current = self.compile_instance_func_call(fc, &instance)?;
                                            continue;
                                        }
                                        x => unreachable!("{:?}", x),
                                    },
                                    None => return Err(compiler_error!(self, "[ER4] Could not resolve type `{}`", ident.to_string())),
                                },
                            };
                            match &next_member.value.token {
                                Expr::Ident(field_name) => {
                                    let member = self.resolve_class_member_ptr(&instance, &class_impl, field_name)?;
                                    let val = member.load(self)?;
                                    current = TypedValue {
                                        val,
                                        ty: member.get_type(),
                                    };
                                }
                                Expr::Unary(unary) => match &unary.value.token {
                                    Expr::Ident(field_name) => {
                                        let member = self.resolve_class_member_ptr(&instance, &class_impl, field_name)?;
                                        let expr = member.load(self)?;
                                        current = self.compile_unary_expr(
                                            unary.op,
                                            &TypedValue {
                                                val: expr,
                                                ty: member.get_type(),
                                            },
                                        )?;
                                    }
                                    x => unreachable!("{:?}", x),
                                },
                                Expr::FuncCall(fc) => {
                                    current = self.compile_instance_func_call(fc, &instance)?;
                                }
                                x => unreachable!("{:?}", x),
                            }
                        }
                        (MemberType::Class, _) => {
                            return Err(compiler_error!(self, "[ER8] The `.` operator is forbidden on type `{}`", current.ty.to_string()))
                        }
                        (MemberType::Array, _) => {
                            return Err(compiler_error!(self, "[ER9] The `[]` operator is forbidden on type `{}`", current.ty.to_string()))
                        }
                        (x, y) => unreachable!("x = {:?}, y = {:?}", x, y),
                    }
                }

                current
            }
            Expr::Range(range) => {
                let obj =
                    self.instantiate_object(BasicType::Object(GenericIdentifier::from_name("core::collections::Range")).to_complex())?;
                let class_impl = self.cpl.type_provider.get_class_by_name(&GenericIdentifier::from_complex_type(&obj.ty)).unwrap();

                let start = self.compile_expr(&range.start, Some(&BasicType::Int32.to_complex()))?;
                let end = self.compile_expr(&range.end, Some(&BasicType::Int32.to_complex()))?;

                let start_ptr = self.resolve_class_member_ptr(&obj, &class_impl, &Identifier::from_string("start"))?;
                let end_ptr = self.resolve_class_member_ptr(&obj, &class_impl, &Identifier::from_string("end"))?;
                let current_ptr = self.resolve_class_member_ptr(&obj, &class_impl, &Identifier::from_string("current"))?;

                start_ptr.store(Operator::Equals, self, start.clone())?;
                end_ptr.store(Operator::Equals, self, end)?;
                current_ptr.store(Operator::Equals, self, start)?;

                obj
            }
            Expr::Null => {
                TypedValue::new(ComplexType::Basic(BasicType::Null), self.cpl.context.const_null_ptr(self.cpl.context.get_void_type()))
            }
            Expr::Psuedo(typed_value) => typed_value.clone(),
            Expr::New(new) => self.compile_new_call(new)?,
            Expr::NewArray(arr) => {
                let element_type = self.resolve_type(&arr.element_type.complex)?;
                let length = self.compile_expr(&arr.length, Some(&BasicType::USize.to_complex()))?;
                let initial_value = self.compile_expr(&arr.initial_value, Some(&element_type))?;

                self.compile_new_array(&element_type, &initial_value, &length)?
            }
            Expr::SpecifiedArray(arr) => {
                let element_type = self.resolve_type(&arr.element_type.complex)?;

                let const_length = self.cpl.context.const_int(BasicType::USize.as_llvm_type(self.cpl), arr.initial_values.len() as _);
                let const_null =
                    TypedValue::new(BasicType::Null.to_complex(), self.cpl.context.const_null(element_type.as_llvm_type(self.cpl)));
                let length = TypedValue::new(BasicType::USize.to_complex(), const_length);

                let array_value = self.compile_new_array(&element_type, &const_null, &length)?;
                for i in 0..arr.initial_values.len() {
                    let element_value = self.compile_expr(&arr.initial_values[i], Some(&element_type))?;
                    let const_i = TypedValue::new(
                        BasicType::USize.to_complex(),
                        self.cpl.context.const_int(BasicType::USize.as_llvm_type(self.cpl), i as _),
                    );
                    self.store_array_element(&array_value, &element_value, &const_i, true)?;
                }
                array_value
            }
            Expr::Logic(logic) => {
                let lhs = self.compile_expr(&logic.lhs, None)?;
                if logic.op == Operator::As {
                    let cast_target = match &logic.rhs.token {
                        Expr::CastTarget(target) => target,
                        _ => unreachable!(),
                    };
                    let cast_target = self.resolve_type(&cast_target.complex)?;
                    self.compile_cast(lhs, cast_target)?
                } else {
                    let rhs = self.compile_expr(&logic.rhs, Some(&lhs.ty))?;
                    self.compile_logic_expr(lhs, logic.op, rhs)?
                }
            }
            Expr::BoolLit(value) => {
                let bool_val = self.cpl.context.const_int(BasicType::Bool.as_llvm_type(self.cpl), u64::from(*value));
                TypedValue::new(BasicType::Bool.to_complex(), bool_val)
            }
            Expr::Unary(unary) => {
                let expr = self.compile_expr(&unary.value, None)?;
                self.compile_unary_expr(unary.op, &expr)?
            }
            Expr::Default(default) => {
                let ty = self.resolve_type(&default.complex)?;
                self.compile_default_expr(&ty)?
            }
            Expr::SizeOf(size_of) => {
                let ty = self.resolve_type(&size_of.complex)?;
                self.compile_sizeof_expr(&ty)?
            }
            Expr::EnumWithData(enum_with_data) => {
                let declaring_type = self.resolve_type(&QualifiedType::from_qualifier(&enum_with_data.declaring_type).complex)?;
                let declaring_type = match declaring_type {
                    ComplexType::Basic(BasicType::Object(ident)) => ident,
                    _ => return Err(compiler_error!(self, "Type `{}` is not an enum type", declaring_type.to_string())),
                };
                self.compile_new_enum_member(&declaring_type, &enum_with_data.member.token.0, Some(enum_with_data.data.clone()))?
            }
            Expr::Match(mtch) => self.compile_match_expr(mtch)?,
            x => unimplemented!("{:#?}", x),
        })
    }

    fn compile_match_expr(&mut self, mtch: &MatchExpr) -> Result<TypedValue> {
        let value = self.compile_expr(&mtch.value, None)?;

        let mut match_result_types = Vec::new();
        for branch in &mtch.branches {
            llvm::set_eval_only(true);

            let mut temp_block = self.state.new_block(&mut self.builder);
            match &branch.arg {
                MatchExprBranchArg::EnumWithData {
                    member,
                    ..
                } => {
                    let resolved = self.resolve_enum_variant(&GenericIdentifier::from_complex_type(&value.ty), &member.token.0)?;
                    match resolved.data_type {
                        Some(_) => {
                            for field in resolved.data_type.unwrap() {
                                temp_block.locals.push(LocalVar {
                                    name: field.name,
                                    value: TypedValue::new(field.ty, self.cpl.context.const_null_ptr(self.cpl.context.get_void_type())),
                                });
                            }
                        }
                        None => return Err(compiler_error_loc!(&member.loc, "Enum variant `{}` takes no associated data", member.token.0)),
                    }
                }
                _ => (),
            }

            self.state.block_stack.push(temp_block);
            match &branch.statement.token {
                Statement::Expr(expr) => {
                    let result = self.compile_expr(expr, None)?;
                    match_result_types.push((result.ty, expr.loc.clone()));
                }
                _ => {
                    let returns = self.compile_block_statement(&[branch.statement.clone()], BlockType::Generic);
                    if !returns {
                        match_result_types.push((BasicType::Void.to_complex(), branch.statement.loc.clone()));
                    }
                }
            }
            self.state.block_stack.pop();

            llvm::set_eval_only(false);
        }

        for (ty, loc) in &match_result_types {
            for (other_ty, _) in &match_result_types {
                if !self.cpl.type_provider.is_assignable_to(ty, other_ty) {
                    return Err(compiler_error_loc!(loc, "Mismatched types `{}` and `{}`", ty.to_string(), other_ty.to_string()));
                }
            }
        }

        let mut prealloc_result =
            TypedValue::new(BasicType::Void.to_complex(), self.cpl.context.const_null_ptr(self.cpl.context.get_void_type()));
        if !match_result_types.is_empty() && match_result_types[0].0 != BasicType::Void.to_complex() {
            let allocated = self.emit(Insn::Alloca(match_result_types[0].0.as_llvm_type(self.cpl)));
            prealloc_result = TypedValue::new(match_result_types[0].0.clone(), allocated);
        }

        let final_rotated_parent = self.state.new_rotated_parent(&mut self.builder);
        for (i, branch) in mtch.branches.iter().enumerate() {
            let statement_block = self.state.new_block(&mut self.builder);
            let rotated_parent = if i == mtch.branches.len() - 1 {
                final_rotated_parent.clone()
            } else {
                self.state.new_rotated_parent(&mut self.builder)
            };

            match &branch.arg {
                MatchExprBranchArg::Catchall(loc) => {
                    if i != mtch.branches.len() - 1 {
                        return Err(compiler_error_loc!(loc, "Catchall branch must be the last branch in the match"));
                    }

                    self.emit(Insn::Br(statement_block.llvm_block.as_val()));
                }
                MatchExprBranchArg::EnumWithData {
                    member,
                    ..
                }
                | MatchExprBranchArg::Enum(member) => {
                    let resolved = self.resolve_enum_variant(&GenericIdentifier::from_complex_type(&value.ty), &member.token.0)?;
                    let test_variant_id_ptr = self.emit(Insn::GetElementPtr(value.val, value.ty.as_llvm_type(self.cpl), 0));
                    let test_variant_id = self.emit(Insn::Load(test_variant_id_ptr, self.cpl.context.get_i32_type()));
                    let real_variant_id_const = self.cpl.context.const_int(self.cpl.context.get_i32_type(), resolved.variant_id as _);
                    let are_equal = self.emit(Insn::ICmp(LLVMIntPredicate::LLVMIntEQ, test_variant_id, real_variant_id_const));

                    self.emit(Insn::CondBr(are_equal, statement_block.llvm_block.as_val(), rotated_parent.llvm_block.as_val()));
                }
                MatchExprBranchArg::Expr(expr) => {
                    let to_match = self.compile_expr(expr, Some(&value.ty))?;
                    self.assert_assignable_to(&to_match.ty, &value.ty)?;

                    let are_equal = self.compile_logic_expr(value.clone(), Operator::Equals, to_match)?;
                    self.assert_assignable_to(&are_equal.ty, &BasicType::Bool.to_complex())?;

                    self.emit(Insn::CondBr(are_equal.val, statement_block.llvm_block.as_val(), rotated_parent.llvm_block.as_val()));
                }
            }

            self.builder.append_block(&statement_block.llvm_block);
            self.state.push_block(&mut self.builder, statement_block);

            match &branch.arg {
                MatchExprBranchArg::EnumWithData {
                    member,
                    ..
                } => {
                    let resolved = self.resolve_enum_variant(&GenericIdentifier::from_complex_type(&value.ty), &member.token.0)?;

                    let specific_variant_type =
                        self.cpl.context.get_abi_enum_type_specific_element(self.cpl, &resolved.enum_impl, resolved.variant_id);
                    let enum_ptr = self.emit(Insn::BitCast(value.val, self.cpl.context.get_pointer_type(specific_variant_type)));
                    for (i, field) in resolved.data_type.unwrap().into_iter().enumerate() {
                        let field_ptr = self.emit(Insn::GetElementPtr(enum_ptr, specific_variant_type, (i + 1) as u32));
                        // let field_val = TypedValueContainer(TypedValue::new(field.ty.clone(), field_ptr)).load(self)?;
                        self.state.get_current_block_mut().locals.push(LocalVar {
                            name: field.name,
                            value: TypedValue::new(field.ty, field_ptr),
                        });
                    }
                }
                _ => (),
            }

            match &branch.statement.token {
                Statement::Expr(expr) => {
                    let result = self.compile_expr(expr, None)?;
                    self.copy(&result, &prealloc_result)?;

                    self.pop_block()?;
                    self.emit(Insn::Br(final_rotated_parent.llvm_block.as_val()));
                }
                _ => {
                    let returns = self.compile_block_statement(&[branch.statement.clone()], BlockType::Generic);
                    if !returns {
                        self.pop_block()?;
                        self.emit(Insn::Br(final_rotated_parent.llvm_block.as_val()));
                    }
                }
            }

            self.builder.append_block(&rotated_parent.llvm_block);
            self.builder.use_block(&rotated_parent.llvm_block);
        }

        // self.builder.use_block(&final_rotated_parent.llvm_block);
        // final_rotated_parent.llvm_block.append();

        let loaded_result = match prealloc_result.ty {
            ComplexType::Basic(BasicType::Void) => prealloc_result.val,
            _ => TypedValueContainer(prealloc_result.clone()).load(self)?,
        };
        Ok(TypedValue::new(prealloc_result.ty, loaded_result))
    }

    /// Returns a pointer to the local identifier, static field, or enum resolved by this function.
    fn resolve_static_field_reference(&mut self, sfr: &Qualifier) -> Result<TypedValue> {
        self.loc(&sfr.get_location());

        if sfr.0.len() == 1 {
            match self.resolve_ident(&sfr.0[0]) {
                Ok(var) => return Ok(var.value),
                Err(_) => (),
            }
        }

        let field_name = sfr.to_string();
        let (field_name, field_type) = match self.cpl.type_provider.get_static_field_by_name(&field_name) {
            Some(field) => (field_name, field),
            None => {
                let namespaced_name = format!("{}::{}", self.get_source_function().namespace_name, field_name);
                match self.cpl.type_provider.get_static_field_by_name(&namespaced_name) {
                    Some(field) => (namespaced_name, field),
                    None => {
                        if sfr.0.len() > 1 {
                            let declaring_type = Qualifier(sfr.0[0..sfr.0.len() - 1].to_vec());
                            let enum_name = GenericIdentifier::from_name(&declaring_type.to_string());
                            let member = sfr.0[sfr.0.len() - 1].token.0.clone();
                            match self.cpl.type_provider.get_enum_by_name(&enum_name) {
                                Some(_) => return self.compile_new_enum_member(&enum_name, &member, None),
                                None => {
                                    let namespaced_name =
                                        format!("{}::{}", self.get_source_function().namespace_name, declaring_type.to_string());
                                    let enum_name = GenericIdentifier::from_name(&namespaced_name);
                                    match self.cpl.type_provider.get_enum_by_name(&enum_name) {
                                        Some(_) => return self.compile_new_enum_member(&enum_name, &member, None),
                                        None => (),
                                    }
                                }
                            };
                        }

                        return Err(compiler_error!(self, "No such enum member, static field, or local identifier `{}`", field_name));
                    }
                }
            }
        };

        let global = self.unit.mdl.get_or_extern_global(&GlobalVariable {
            name: field_name,
            ty: field_type.as_llvm_type(self.cpl),
        });
        Ok(TypedValue::new(field_type, global))
    }

    fn resolve_enum_variant(&mut self, declaring_type: &GenericIdentifier, member: &str) -> Result<ResolvedEnumMember> {
        let enum_impl = match self.cpl.type_provider.get_enum_by_name(declaring_type) {
            Some(enum_impl) => enum_impl,
            None => {
                let namespaced_name = format!("{}::{}", self.get_source_function().namespace_name, declaring_type.name);
                let namespaced_name = GenericIdentifier::from_name_with_args(&namespaced_name, &declaring_type.generic_args);
                match self.cpl.type_provider.get_enum_by_name(&namespaced_name) {
                    Some(enum_impl) => enum_impl,
                    None => return Err(compiler_error!(self, "No such enum type `{}`", declaring_type.to_string())),
                }
            }
        };
        match enum_impl.elements.iter().enumerate().find(|element| element.1.name == member) {
            Some(member) => Ok(ResolvedEnumMember {
                enum_impl: enum_impl.clone(),
                variant_id: member.0,
                data_type: member.1.data.clone(),
            }),
            None => Err(compiler_error!(self, "No such enum member `{}` in type `{}`", member, declaring_type.to_string())),
        }
    }

    fn compile_new_enum_member(
        &mut self,
        declaring_type: &GenericIdentifier,
        member: &str,
        data: Option<Vec<NewCallField>>,
    ) -> Result<TypedValue> {
        let resolved = self.resolve_enum_variant(declaring_type, member)?;
        if data.is_none() && let Some(data_ty) = resolved.data_type {
            return Err(compiler_error!(self, "Enum member `{}::{}` required associated data type `{}` but no data was provided", declaring_type.name, member, BasicType::AnonymousStruct(data_ty).to_string()));
        }
        if resolved.data_type.is_none() && data.is_some() {
            return Err(compiler_error!(self, "Enum member `{}::{}` has no associated data", declaring_type.name, member,));
        }

        let any_variant_type = self.cpl.context.get_abi_enum_type_any_element(self.cpl, &resolved.enum_impl);
        let enum_ptr = self.emit(Insn::Alloca(any_variant_type));

        let variant_id_const = self.cpl.context.const_int(self.cpl.context.get_i32_type(), resolved.variant_id as _);
        let variant_id_ptr = self.emit(Insn::GetElementPtr(enum_ptr, any_variant_type, 0));

        self.emit(Insn::Store(variant_id_const, variant_id_ptr));

        if let Some(data) = data {
            if let Some(data_ref) = &resolved.enum_impl.elements[resolved.variant_id].data {
                let specific_variant_type =
                    self.cpl.context.get_abi_enum_type_specific_element(self.cpl, &resolved.enum_impl, resolved.variant_id);
                let enum_ptr = self.emit(Insn::BitCast(enum_ptr, self.cpl.context.get_pointer_type(specific_variant_type)));

                for field in &data {
                    let (field_offset, corresponding_field) =
                        match data_ref.iter().enumerate().find(|e| e.1.name == field.field_name.token.0) {
                            Some(field) => field,
                            None => {
                                return Err(compiler_error!(
                                    self,
                                    "Enum variant `{}` associated data has no field `{}`",
                                    resolved.enum_impl.elements[resolved.variant_id].name,
                                    field.field_name.token.0
                                ))
                            }
                        };
                    let field_ptr = self.emit(Insn::GetElementPtr(
                        enum_ptr,
                        specific_variant_type,
                        (1 + field_offset) as u32, // offset 1 to accomodate for the variant ID
                    ));
                    let arg_value = match &field.value {
                        Some(expr) => self.compile_expr(expr, Some(&corresponding_field.ty))?,
                        None => {
                            let ident = self.resolve_ident(&field.field_name)?;
                            self.load_local_var(&ident)?
                        }
                    };
                    self.store(Operator::Equals, arg_value, &TypedValue::new(corresponding_field.ty.clone(), field_ptr))?;
                }
            } else {
                return Err(compiler_error!(
                    self,
                    "Enum variant `{}` takes no data",
                    resolved.enum_impl.elements[resolved.variant_id].name
                ));
            }
        } else if resolved.enum_impl.elements[resolved.variant_id].data.is_some() {
            return Err(compiler_error!(
                self,
                "Enum variant `{}` takes associated data",
                resolved.enum_impl.elements[resolved.variant_id].name,
            ));
        }

        Ok(TypedValue::new(BasicType::Object(declaring_type.clone()).to_complex(), enum_ptr))
    }

    fn compile_default_expr(&mut self, ty: &ComplexType) -> Result<TypedValue> {
        let result = match ty {
            ComplexType::Basic(basic) => match basic {
                BasicType::Object(ident) => {
                    let (is_classlike, fields) = match self.cpl.type_provider.get_class_by_name(ident) {
                        Some(class) => (matches!(class.class_type, ClassType::Class | ClassType::Interface), class.fields),
                        None => match self.cpl.type_provider.get_enum_by_name(ident) {
                            Some(_) => (true, Vec::new()),
                            None => panic!(),
                        },
                    };
                    if is_classlike {
                        let resolved_interface_impls = self.cpl.type_provider.get_resolved_interface_impls(ident);
                        for resolved_interface_impl in resolved_interface_impls {
                            let source_interface = self.cpl.type_provider.get_source_interface_impl(&resolved_interface_impl);
                            if source_interface.interface_name == "core::object::Default" {
                                let interface_id =
                                    self.cpl.type_provider.get_resolved_interface_id(&GenericIdentifier::from_name_with_args(
                                        &source_interface.interface_name,
                                        &resolved_interface_impl.interface_generic_impls,
                                    ));
                                let create_default_ptr =
                                    self.get_interface_method_ptr(&InterfaceInvocation::Static(ty.clone()), interface_id, 0)?;
                                let callable = self
                                    .cpl
                                    .type_provider
                                    .get_function_by_name(
                                        &GenericIdentifier::from_name_with_args("core::object::Default::default", &[]),
                                        &[],
                                    )
                                    .unwrap();
                                let result = self.call_function(create_default_ptr, &callable, &[])?;
                                return Ok(TypedValue::new(ty.clone(), result));
                            }
                        }

                        return Err(compiler_error!(self, "Type `{}` does not implement `core::object::Default`", ident.to_string()));
                    } else {
                        let struct_ty = basic.as_llvm_type(self.cpl);
                        let obj = self.emit(Insn::Alloca(struct_ty));
                        for i in 0..fields.len() {
                            let ptr = self.emit(Insn::GetElementPtr(obj, struct_ty, (i + 1) as _)); // +1 to offset the classinfo pointer

                            let default_value = self.compile_default_expr(&fields[i].ty)?;
                            self.copy(&default_value, &TypedValue::new(default_value.ty.clone(), ptr))?;
                        }
                    }

                    let ty = basic.as_llvm_type(self.cpl);
                    self.cpl.context.const_int(ty, 0)
                }
                _ => {
                    let ty = basic.as_llvm_type(self.cpl);
                    self.cpl.context.const_int(ty, 0)
                }
            },
            _ => panic!(),
        };
        Ok(TypedValue::new(ty.clone(), result))
    }

    fn compile_reference_expr(&mut self, val: &TypedValue) -> Result<TypedValue> {
        Ok(match &val.ty {
            ComplexType::Array(element_type) => {
                let slice_type = self.cpl.context.get_abi_slice_type(element_type.as_llvm_type(self.cpl), &element_type.to_string());
                let array_ptr_ptr = self.emit(Insn::GetElementPtr(val.val, slice_type, 2)); // element 2 is the pointer to the heap data
                let array_data_type =
                    self.cpl.context.get_abi_array_data_type(element_type.as_llvm_type(self.cpl), &element_type.to_string());
                let array_ptr = self.emit(Insn::Load(array_ptr_ptr, self.cpl.context.get_pointer_type(array_data_type)));
                let data_ptr_ptr = self.emit(Insn::GetElementPtr(array_ptr, array_data_type, 1)); // functionally this is like a `T*`, a pointer to an array
                let data_ptr = self.emit(Insn::Load(data_ptr_ptr, BasicType::USize.as_llvm_type(self.cpl)));

                let ty =
                    BasicType::Object(GenericIdentifier::from_name_with_args("core::mem::Pointer", &[*element_type.clone()])).to_complex();
                let pointer_struct = self.instantiate_object(ty)?;
                let address_ptr = self.emit(Insn::GetElementPtr(pointer_struct.val, pointer_struct.ty.as_llvm_type(self.cpl), 1));
                self.emit(Insn::Store(data_ptr, address_ptr));

                pointer_struct
            }
            ty => return Err(compiler_error!(self, "Cannot take reference to type `{}`", ty.to_string())),
        })
    }

    fn compile_dereference_expr(&mut self, val: &TypedValue) -> Result<TypedValue> {
        if !self.is_unsafe() {
            return Err(compiler_error!(self, "Cannot peform unsafe operation `deref` without an `unsafe` block"));
        }

        Ok(match &val.ty {
            ComplexType::Basic(BasicType::Object(ident)) => {
                if ident.name == "core::mem::Pointer" {
                    let ty = self.resolve_type(&ident.generic_args[0])?; // deref Pointer<T> -> T
                    let addr_ptr = self.emit(Insn::GetElementPtr(val.val, val.ty.as_llvm_type(self.cpl), 1)); // pointer to the `address` field in the Pointer<T> struct
                    let addr_int = self.emit(Insn::Load(addr_ptr, BasicType::USize.as_llvm_type(self.cpl))); // this Load loads the address stored in the Pointer<T> struct
                    let addr_ptr = self.emit(Insn::IntToPtr(addr_int, ty.clone().to_reference().as_llvm_type(self.cpl)));
                    let val = self.emit(Insn::Load(addr_ptr, ty.as_llvm_type(self.cpl))); // this Load derefences the pointer

                    TypedValue {
                        ty,
                        val,
                    }
                } else {
                    return Err(compiler_error!(self, "Cannot dereference non-reference type `{}`", val.ty.to_string()));
                }
            }
            _ => return Err(compiler_error!(self, "Cannot dereference non-reference type `{}`", val.ty.to_string())),
        })
    }

    fn compile_integer_literal_expr(&mut self, val: i64, type_hint: Option<&ComplexType>) -> Result<TypedValue> {
        let ty = match type_hint {
            Some(ComplexType::Basic(basic)) => match basic {
                BasicType::Bool
                | BasicType::Void
                | BasicType::Object {
                    ..
                } => &BasicType::Int32,
                numeric => numeric,
            },
            _ => &BasicType::Int32,
        };
        let llvm_type = ty.as_llvm_type(self.cpl);
        Ok(TypedValue::new(ty.clone().to_complex(), self.cpl.context.const_int(llvm_type, val as u64)))
    }

    fn compile_string_literal_expr(&mut self, str: &str) -> Result<TypedValue> {
        let replaced = str.to_owned();
        let replaced = replaced.replace("\\0", "\0").replace("\\n", "\n");
        let str_val = TypedValue::new(
            BasicType::Int8.to_complex().to_reference(), // equivalent to char*
            self.emit(Insn::GlobalString(replaced)),
        );

        let str_len = self.cpl.context.const_int(BasicType::USize.as_llvm_type(self.cpl), str.len() as _);
        let str_len = TypedValue::new(BasicType::USize.to_complex(), str_len);

        self.cpl.queue_function_compilation(
            self.cpl
                .type_provider
                .get_function_by_name(
                    &GenericIdentifier::from_name_with_args("core::array::copyFromPtr", &[BasicType::Char.to_complex()]),
                    &[
                        BasicType::Object(GenericIdentifier::from_name_with_args("core::mem::Pointer", &[BasicType::Char.to_complex()]))
                            .to_complex(),
                        BasicType::USize.to_complex(),
                    ],
                )
                .unwrap(),
        );
        self.cpl.queue_function_compilation(
            self.cpl
                .type_provider
                .get_function_by_name(
                    &GenericIdentifier::from_name_with_args("core::mem::Pointer::to", &[BasicType::Char.to_complex()]),
                    &[BasicType::USize.to_complex()],
                )
                .unwrap(),
        );
        self.cpl.queue_function_compilation(
            self.cpl
                .type_provider
                .get_function_by_name(
                    &GenericIdentifier::from_name_with_args("core::string::String::fromUtf8Slice", &[]),
                    &[BasicType::Char.to_complex().to_array()],
                )
                .unwrap(),
        );

        let new_string_impl = ResolvedFunctionNode::externed(
            "keid_new_string",
            &[BasicType::Int8.to_complex().to_reference(), BasicType::Int64.to_complex()],
            Varargs::None,
            BasicType::Object(GenericIdentifier::from_name("core::string::String")).to_complex(),
        );
        let new_string_ref = self.get_function_ref(&new_string_impl)?;
        let string_instance = self.call_function(new_string_ref, &new_string_impl, &[str_val, str_len])?;
        let string_instance =
            TypedValue::new(BasicType::Object(GenericIdentifier::from_name("core::string::String")).to_complex(), string_instance);

        Ok(string_instance)
    }

    fn compile_sizeof_expr(&mut self, ty: &ComplexType) -> Result<TypedValue> {
        let type_size = self.cpl.context.target.get_type_size(ty.as_llvm_type(self.cpl));
        let size_const = self.cpl.context.const_int(BasicType::USize.as_llvm_type(self.cpl), type_size);
        Ok(TypedValue::new(BasicType::USize.to_complex(), size_const)) // typeof(T): usize
    }

    fn load_local_var(&mut self, var: &LocalVar) -> Result<TypedValue> {
        let val = if var.value.ty.is_struct(&self.cpl.type_provider) {
            var.value.val
        } else {
            self.emit(Insn::Load(var.value.val, var.value.ty.as_llvm_type(self.cpl)))
        };
        Ok(TypedValue {
            ty: var.value.ty.clone(),
            val,
        })
    }
}
